//! Custom titlebar with an integrated tab bar for the GPUI client rebuild.
//!
//! The rebuild has a no-native-decorations mandate: the window is undecorated
//! and this [`TitlebarView`] draws the entire top chrome — a draggable move
//! region, the workspace-badge pills, the tab strip (active accent, per-tab
//! close button, drag-reorder ghost, attention flash, AI activity dot, and the
//! context-% suffix), and the equalize icon. Window controls belong to the
//! native decorations and settings lives in the bottom status bar, so neither
//! is drawn here. It is a `gpui::Entity` so its interactions mutate state and
//! emit [`TitlebarEvent`]s the shell reacts to; the pure layout/decay math lives
//! in [`crate::tab_bar`].

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, BoxShadow, Context, DragMoveEvent, ElementId,
    EventEmitter, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Rgba, Role, Window, WindowControlArea, div, ease_out_quint, hsla,
    prelude::*, px,
};
use scribe_common::ids::{SessionId, WorkspaceId};

use crate::tab_bar::{
    GroupBadge, TabBarColors, TabData, TabShown, flash_blend, lifted_tone, pill_fill, px_units,
    reorder_target_index, tab_display_title,
};
use crate::tab_drop_motion::{GapSlot, TabDropMotion, TabDropTarget};
use crate::workspace_drag::{EmptyWorkspaceDragGhost, WorkspaceDragMarker};

/// Approximate advance width of one label character.
const CHAR_WIDTH: f32 = 8.0;

/// Leading tab mark shown while the server holds an agent activity lease.
const AGENT_ACTIVE_GLYPH: &str = "◆";

/// The width a tab starts from before it flexes: its basis, and the fallback
/// the drag geometry uses until a frame has measured a real one.
pub const TAB_WIDTH: f32 = 176.0;

/// Floor a shrinking tab stops at: room for the close button plus a few title
/// characters, so a crowded group bar compresses tabs instead of slicing
/// their glyphs at the clip edge. Shared with the shell's in-region bars so a
/// crowded lower region compresses identically.
pub const TAB_MIN_WIDTH: f32 = 56.0;

/// Ceiling a growing tab stops at (the layered chrome mock's `max-width`).
/// Tabs still divide their run equally, but a lone tab no longer stretches
/// across a wide window; the run's remaining width stays bare strip. Shared
/// with the shell's in-region bars and with the slot math behind insertion
/// targets, so a capped tab's slot is the one the drop lands in.
pub const TAB_MAX_WIDTH: f32 = 230.0;

/// Vertical inset between the strip's top edge and a tab's raised card. The
/// strip shows through this gap, so the card's rounded top corners read as a
/// browser-style physical tab instead of a flat full-height band segment.
/// Workspace pills share the same inset and bottom-flush height so pills and
/// tab cards sit on one line, as in the layered chrome mock.
pub const TAB_CARD_TOP_GAP: f32 = 5.0;

/// Horizontal inset of a bar's pill-and-tab run from its region's edges.
///
/// The pane card below sits this far inside its region (it is the shell's
/// `PANE_CARD_MARGIN`), so a run inset by the same amount puts every tab card
/// over the pane card's straight top edge: the open tab can then bridge into
/// its pane without overhanging the card's side or rounding into its corner.
pub const TAB_RUN_INSET: f32 = 6.0;

/// The raised card behind an active, hovered, or flashing tab.
///
/// The fill is an absolute child rather than a full-bleed element background:
/// inset `TAB_CARD_TOP_GAP` from the top, the strip shows above its rounded
/// corners and the shape reads as a browser-style tab. The card runs to the
/// band's bottom edge — and the active card wears the pane fill plus, through
/// `edge`, the pane card's own outline on its top and sides, so the active tab
/// reads as the top of the pane below it rather than a box on the strip. The
/// grid finishes that join with [`crate::main`]'s tab bridge over the gap.
/// Hover and flash cards pass no edge and float on the strip.
#[must_use]
pub fn tab_card(bg: Rgba, edge: Option<Rgba>) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(TAB_CARD_TOP_GAP))
        .bottom_0()
        .rounded_t(px(6.0))
        .bg(bg)
        .when_some(edge, |card, edge| {
            card.border_t_1().border_l_1().border_r_1().border_color(edge)
        })
        .into_any_element()
}

/// The keyboard mark on the window-focused region's open tab: a short accent
/// bar leading the title, in the workspace's effective accent.
#[must_use]
pub fn focus_mark(accent: Rgba) -> AnyElement {
    div().flex_none().w(px(3.0)).h(px(10.0)).rounded(px(1.5)).mr_2().bg(accent).into_any_element()
}

/// The outline an open tab's card shares with its pane: the region accent
/// where the pane wears its focus ring, the whisper edge elsewhere, nothing
/// for a tab that is not open.
#[must_use]
pub fn open_tab_edge(tab: &TabData, colors: &TabBarColors) -> Option<Rgba> {
    if tab.is_focused() {
        Some(tab.group_accent.unwrap_or(colors.accent))
    } else if tab.is_active() {
        Some(colors.pane_edge)
    } else {
        None
    }
}

/// Pointer travel, in pixels, required before an armed press on the move region
/// hands the window to the compositor. Absorbs the jitter of an ordinary click
/// so a wobbly press on empty chrome does not turn into a window move.
const WINDOW_MOVE_THRESHOLD: f32 = 4.0;

/// How a tab activation was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabActivationSource {
    Pointer,
    Keyboard,
}

/// Workspace pill identity as seen by the titlebar entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarWorkspaceDragSource {
    /// A pill attached to the first tab in a titlebar workspace run.
    TabIndex(usize),
    /// A standalone pill for a top-row workspace with no tabs.
    Workspace(WorkspaceId),
}

/// Events the titlebar emits for the shell to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarEvent {
    /// A tab was activated; make it active and react to the input source.
    SelectTab { index: usize, source: TabActivationSource },
    /// A tab's close button was clicked.
    CloseTab(usize),
    /// A tab's magnifier was clicked: open its focused pane in a mirror window.
    MirrorTab(usize),
    /// A tab finished a drag-reorder from `from` to `to`.
    ReorderTab { from: usize, to: usize },
    /// The equalize icon was clicked (equalize the active tab's panes).
    Equalize,
    /// Pointer entered or left a detected workspace's Beads icon.
    BeadsHover { index: usize, hovered: bool },
    /// A detected workspace's Beads icon was clicked.
    ToggleBeadsBoard { index: usize },
    /// A pill without a tab was clicked; focus its empty workspace region.
    FocusWorkspace(WorkspaceId),
    /// A workspace pill press armed the dedicated workspace drag marker.
    ArmWorkspaceDrag(TitlebarWorkspaceDragSource),
    /// Pointer entered or left a standalone pill's Beads icon.
    StandaloneBeadsHover { workspace_id: WorkspaceId, hovered: bool },
    /// A standalone pill's Beads icon was clicked.
    ToggleStandaloneBeadsBoard(WorkspaceId),
}

/// Value handed to GPUI's native drag system when a titlebar tab press turns
/// into a drag. Its presence as the active drag keeps mouse-move events
/// flowing to [`TitlebarView`]'s `on_drag_move` listener anywhere in the
/// window, so the drag survives the cursor leaving the titlebar band. It
/// carries the ghost GPUI paints under the cursor.
pub struct TabDrag(pub TabDragGhost);

/// The cursor-following copy of a dragged tab, painted by GPUI's native drag
/// overlay at the grab point for as long as the drag lives.
///
/// The overlay is positioned by the press offset inside the tab's hitbox, so a
/// ghost of the tab's own size sits exactly where the tab was under the
/// pointer and then follows it anywhere in the window, including over the bar
/// it will be dropped on. The slot it left stays as a quiet recess while the
/// pointer is in the source bar and closes once the tab is aimed elsewhere.
#[derive(Clone)]
pub struct TabDragGhost {
    pub title: String,
    pub colors: TabBarColors,
    pub width: f32,
    pub height: f32,
    /// Where inside the ghost the pointer holds it, recorded by the `on_drag`
    /// constructor from GPUI's cursor offset. The release settle starts the
    /// card from exactly where GPUI last painted it.
    pub grab: Rc<Cell<(f32, f32)>>,
}

impl TabDragGhost {
    /// The ghost card itself, sized like the tab it lifts.
    ///
    /// One definition for every painter: GPUI's drag overlay in the source
    /// window, a sibling window that paints the same card under the pointer
    /// once a cross-window drag has left the source surface (GPUI cannot paint
    /// a drag view outside its origin window), and the release settle.
    ///
    /// A lifted card: fully opaque, so neither the strip nor the tab under it
    /// bleeds through the title, rounded on all four corners because it no
    /// longer joins a pane, filled a step above the strip ([`lifted_tone`])
    /// and outlined in the accent so it reads on a black theme too, with an
    /// accent cap across its top and a two-layer drop shadow that separates
    /// it from the chrome it floats over.
    #[must_use]
    pub fn element(&self) -> gpui::Div {
        let colors = self.colors;
        div().w(px(self.width)).h(px(self.height)).pt(px(TAB_CARD_TOP_GAP)).child(
            div()
                .relative()
                .size_full()
                .flex()
                .items_center()
                .px_2()
                .rounded(px(6.0))
                .border_1()
                .border_color(Rgba { a: 0.75, ..colors.accent })
                .bg(lifted_tone(&colors))
                .text_color(colors.active_text)
                .text_xs()
                .overflow_hidden()
                .shadow(vec![
                    BoxShadow::new(px(0.0), px(6.0), hsla(0.0, 0.0, 0.0, 0.5))
                        .blur_radius(px(16.0)),
                    BoxShadow::new(px(0.0), px(1.0), hsla(0.0, 0.0, 0.0, 0.4)).blur_radius(px(3.0)),
                ])
                .child(div().absolute().top_0().left_0().right_0().h(px(2.0)).bg(colors.accent))
                .child(div().flex_1().truncate().child(self.title.clone())),
        )
    }
}

impl Render for TabDragGhost {
    /// The pickup: the card rises a few pixels out of the strip as it is lifted.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.element().with_animation(
            "tab-ghost-lift",
            Animation::new(Duration::from_millis(TAB_DRAG_ANIMATION_MS))
                .with_easing(ease_out_quint()),
            |card, lift| card.mt(px(-GHOST_LIFT * lift)),
        )
    }
}

/// How long the drag's pickup, slot open/close, and release settle run.
pub const TAB_DRAG_ANIMATION_MS: u64 = 140;

/// How far the ghost rises out of the strip when picked up.
pub const GHOST_LIFT: f32 = 3.0;

/// What the shell asks the strip to show for a tab drag it is resolving.
///
/// `landings` counts released tabs that have arrived; a new count snaps the
/// strip's gaps shut on the frame the tab itself appears in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TitlebarDropPreview {
    pub target: TabDropTarget,
    pub landings: u64,
}

/// A landing gap `width` wide: the slot a dragged tab will land in, shown as
/// an accent hairline card over a faint accent tint (a solid accent tone read
/// as a slab louder than the tab being dropped). Its width is animated by the
/// caller's [`TabDropMotion`], so it can open, close, and slide between slots.
#[must_use]
pub fn drop_gap(width: f32, colors: &TabBarColors) -> AnyElement {
    let accent = colors.accent;
    div()
        .flex_none()
        .relative()
        .h_full()
        .w(px(width))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left(px(2.0))
                .right(px(2.0))
                .top(px(TAB_CARD_TOP_GAP))
                .bottom_0()
                .rounded_t(px(6.0))
                .border_1()
                .border_color(Rgba { a: 0.6, ..accent })
                .bg(Rgba { a: 0.1, ..accent }),
        )
        .into_any_element()
}

/// A tab's slot narrowed to `width` while it closes (the tab is aimed at
/// another bar, or was released and is on its way) or reopens. The dragged
/// tab's own slot keeps its recess; a released tab's is bare.
#[must_use]
pub fn narrowed_slot(width: f32, recess: Option<&TabBarColors>) -> AnyElement {
    div()
        .flex_none()
        .relative()
        .h_full()
        .w(px(width))
        .overflow_hidden()
        .children(recess.map(drag_placeholder))
        .into_any_element()
}

/// The chip that rides beside the pointer while a tab drag is armed to tear
/// out: it names the outcome a release will have, because nothing else in the
/// window can show a window that does not exist yet.
#[must_use]
pub fn new_window_chip(colors: &TabBarColors) -> gpui::Div {
    div()
        .px_2()
        .py_0p5()
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.accent)
        .bg(colors.active_bg)
        .text_color(colors.active_text)
        .text_xs()
        .child("New window")
}

/// The slot a dragged tab occupies while the pointer is still in its own bar:
/// a quiet recess in the strip's own tone, so the eye stays on the card in
/// hand. It is where the tab goes back if released here.
#[must_use]
pub fn drag_placeholder(colors: &TabBarColors) -> AnyElement {
    div()
        .absolute()
        .left(px(2.0))
        .right(px(2.0))
        .top(px(TAB_CARD_TOP_GAP))
        .bottom_0()
        .rounded_t(px(6.0))
        .bg(pill_fill(colors))
        .into_any_element()
}

/// In-flight tab drag.
#[derive(Debug, Clone, Copy)]
struct DragState {
    /// Index where this drag began, retained for an atomic cross-region move.
    origin: usize,
    /// Index of the tab being dragged (updated as it crosses neighbours).
    source: usize,
    /// Left edge of the first tab, in window pixels.
    origin_x: f32,
    /// Cursor X minus the dragged tab's left edge at grab time.
    grab_offset: f32,
    /// Current cursor X in window pixels.
    cursor_x: f32,
    /// Whether the cursor actually crossed into another tab slot.
    reordered: bool,
    /// Whether the pointer ever left the dragged tab's own group bar. Such a
    /// drag was aimed somewhere else, so its release is never a click.
    left_bar: bool,
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
    visible: bool,
    focused: bool,
}

struct TabChildren {
    display: String,
    agent_indicator: Option<AnyElement>,
    ai_indicator: Option<AnyElement>,
    suffix: Option<AnyElement>,
    mirror: Option<AnyElement>,
    close: Option<AnyElement>,
    mark: Option<AnyElement>,
}

impl TabChildren {
    /// The tab's content, left to right.
    fn attach(self, tab: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
        tab.children(self.mark)
            .children(self.agent_indicator)
            .children(self.ai_indicator)
            // `truncate` keeps the title a single clipped line; without it a
            // title that outgrows the flexed slot (the AI dot appearing, a
            // narrow group bar) wraps to a second line inside the fixed-height
            // tab and the visible text rides up.
            .child(div().flex_1().truncate().child(self.display))
            .children(self.suffix)
            .children(self.mirror)
            .children(self.close)
    }
}

#[derive(Clone, Copy)]
struct TabRender<'a> {
    index: usize,
    tab: &'a TabData,
    foreground: Rgba,
    id: &'a ElementId,
    focused: bool,
    control_focus: TabControlFocus,
}

/// Which of a tab's own controls holds keyboard focus, if either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabControlFocus {
    None,
    Mirror,
    Close,
}

/// A workspace pill whose region currently has no terminal tabs.
#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneBadge {
    /// Workspace region the pill focuses or opens through Beads.
    pub workspace_id: WorkspaceId,
    /// Window-relative left edge of the workspace region.
    pub region_x: f32,
    /// Existing pill data, independent of any terminal tab.
    pub badge: GroupBadge,
}

/// The custom titlebar view.
pub struct TitlebarView {
    colors: TabBarColors,
    height: f32,
    tabs: Vec<TabData>,
    standalone_badges: Vec<StandaloneBadge>,
    show_equalize: bool,
    hovered_tab: Option<usize>,
    drag: Option<DragState>,
    /// A workspace pill owns the current press. Kept separately from tab-drag
    /// state so the titlebar never hands that press to the compositor.
    workspace_drag_armed: bool,
    /// Press origin recorded by a left press on the move region, or `None` when
    /// unarmed. Once the pointer travels [`WINDOW_MOVE_THRESHOLD`] px from it,
    /// the window is handed to the compositor via [`Window::start_window_move`].
    /// `WindowControlArea::Drag` is a no-op on Linux, so this is the real path.
    move_arm: Option<Point<Pixels>>,
    focus_handle: FocusHandle,
    tab_focus_handles: Vec<FocusHandle>,
    tab_mirror_focus_handles: Vec<FocusHandle>,
    tab_close_focus_handles: Vec<FocusHandle>,
    equalize_focus_handle: FocusHandle,
    beads_focus_handles: Vec<FocusHandle>,
    standalone_beads_focus_handles: Vec<FocusHandle>,
    /// Painted `(left, width)` of every tab, by strip index.
    ///
    /// Tabs flex to fill their group, so their geometry is a layout result
    /// rather than a constant, and side-by-side regions give each group its
    /// own width. Drag-reorder geometry has to use what the user is looking
    /// at, not the basis it started from.
    measured_tabs: std::rc::Rc<std::cell::RefCell<Vec<(f32, f32)>>>,
    /// The shell's resolved tab drag: the landing gap and closed slot.
    drop_preview: TitlebarDropPreview,
    /// The animated widths `drop_preview` is moving toward, and the landing
    /// count they last snapped for.
    drop_motion: TabDropMotion,
    drop_landings_seen: u64,
}

impl EventEmitter<TitlebarEvent> for TitlebarView {}

impl TitlebarView {
    /// Create a titlebar bound to the given chrome colors and resolved row height.
    pub fn new(colors: TabBarColors, height: f32, cx: &mut Context<Self>) -> Self {
        Self {
            colors,
            height,
            tabs: Vec::new(),
            standalone_badges: Vec::new(),
            show_equalize: false,
            hovered_tab: None,
            drag: None,
            workspace_drag_armed: false,
            move_arm: None,
            focus_handle: cx.focus_handle(),
            tab_focus_handles: Vec::new(),
            tab_mirror_focus_handles: Vec::new(),
            tab_close_focus_handles: Vec::new(),
            equalize_focus_handle: cx.focus_handle().tab_stop(true),
            beads_focus_handles: Vec::new(),
            standalone_beads_focus_handles: Vec::new(),
            measured_tabs: std::rc::Rc::default(),
            drop_preview: TitlebarDropPreview::default(),
            drop_motion: TabDropMotion::default(),
            drop_landings_seen: 0,
        }
    }

    /// Adopt the shell's drop preview; returns whether anything changed.
    pub fn set_drop_preview(&mut self, preview: TitlebarDropPreview) -> bool {
        if preview == self.drop_preview {
            return false;
        }
        self.drop_preview = preview;
        true
    }

    /// Advance the drop motion to this frame, and keep frames coming while it
    /// moves. Motion off (`reduce_motion`) lands every width at once.
    fn advance_drop_motion(&mut self, now: std::time::Instant, window: &Window, cx: &App) {
        if self.drop_preview.landings != self.drop_landings_seen {
            self.drop_landings_seen = self.drop_preview.landings;
            self.drop_motion.snap_gaps_shut();
        }
        let duration = if cx.reduce_motion() {
            Duration::ZERO
        } else {
            Duration::from_millis(TAB_DRAG_ANIMATION_MS)
        };
        self.drop_motion.update(self.drop_preview.target, now, duration);
        if self.drop_motion.animating(now) {
            window.request_animation_frame();
        }
    }

    /// Arm a tab drag from the press that starts it.
    fn tab_press_listener(
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |this, event: &MouseDownEvent, _win, ctx| {
            let cursor_x = f32::from(event.position.x);
            let origin_x = this.tabs_origin_x();
            this.begin_drag(index, cursor_x, origin_x, ctx);
        })
    }

    /// A zero-ink overlay that records the tab it sits in.
    ///
    /// Tabs flex, so their geometry is a layout result. The drag geometry
    /// needs what the user is looking at, and this is the only place the
    /// painted value exists.
    fn tab_geometry_probe(&self, index: usize) -> AnyElement {
        let measured = std::rc::Rc::clone(&self.measured_tabs);
        gpui::canvas(
            move |bounds, _window, _cx| {
                if let Some(slot) = measured.borrow_mut().get_mut(index) {
                    *slot = (f32::from(bounds.origin.x), f32::from(bounds.size.width));
                }
            },
            |_, (), _window, _cx| {},
        )
        // Pinned to every edge so the record is the tab's own box, not its
        // padding-inset content box: the card and the bridge both span the
        // full tab.
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// `(left, width)` of the tab at `index`: measured when a frame has
    /// painted, the fixed basis from `origin_x` before that.
    fn tab_geometry(&self, index: usize, origin_x: f32) -> (f32, f32) {
        let measured = self.measured_tabs.borrow().get(index).copied();
        match measured {
            Some((left, width)) if left.is_finite() && width >= TAB_MIN_WIDTH => (left, width),
            _ => (origin_x + px_units(index) * TAB_WIDTH, TAB_WIDTH),
        }
    }

    /// The half-open run of strip indices sharing `index`'s workspace region.
    ///
    /// A drag reorders inside this run only: the strip holds every top-row
    /// region's tabs, and crossing into another region's run is a tab-subtree
    /// move the shell commits on release, not a reorder.
    fn group_of(&self, index: usize) -> (usize, usize) {
        workspace_group_ranges(&self.tabs)
            .into_iter()
            .find(|&(start, end)| (start..end).contains(&index))
            .unwrap_or((0, self.tabs.len()))
    }

    /// Release the titlebar's workspace-pill press ownership after Escape,
    /// blur, or source disappearance, where no titlebar mouse-up may arrive.
    pub fn end_workspace_drag(&mut self) {
        self.workspace_drag_armed = false;
    }

    /// Swap the chrome palette, e.g. after a live theme edit is hot-reloaded.
    pub fn set_colors(&mut self, colors: TabBarColors, cx: &mut Context<Self>) {
        self.colors = colors;
        cx.notify();
    }

    /// Apply a live `tab_height + tab_bar_padding` edit to this retained view.
    pub fn set_height(&mut self, height: f32, cx: &mut Context<Self>) {
        if (self.height - height).abs() <= f32::EPSILON {
            return;
        }
        self.height = height;
        cx.notify();
    }

    /// Height this view reserves and paints.
    pub const fn height(&self) -> f32 {
        self.height
    }

    /// The chrome palette currently painted.
    pub const fn colors(&self) -> &TabBarColors {
        &self.colors
    }

    /// Replace the tab strip.
    pub fn set_tabs(&mut self, tabs: Vec<TabData>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        while self.tab_focus_handles.len() < self.tabs.len() {
            self.tab_focus_handles.push(cx.focus_handle().tab_index(0).tab_stop(true));
            self.tab_mirror_focus_handles.push(cx.focus_handle().tab_index(0).tab_stop(true));
            self.tab_close_focus_handles.push(cx.focus_handle().tab_index(0).tab_stop(true));
            self.beads_focus_handles.push(cx.focus_handle().tab_index(0).tab_stop(true));
        }
        self.tab_focus_handles.truncate(self.tabs.len());
        self.tab_mirror_focus_handles.truncate(self.tabs.len());
        self.tab_close_focus_handles.truncate(self.tabs.len());
        self.beads_focus_handles.truncate(self.tabs.len());
        cx.notify();
    }

    /// Borrow the current tabs.
    pub fn tabs(&self) -> &[TabData] {
        &self.tabs
    }

    /// Replace the standalone pills for top-row regions without tabs.
    pub fn set_standalone_badges(&mut self, badges: Vec<StandaloneBadge>, cx: &mut Context<Self>) {
        if self.standalone_badges == badges {
            return;
        }
        self.standalone_badges = badges;
        while self.standalone_beads_focus_handles.len() < self.standalone_badges.len() {
            self.standalone_beads_focus_handles.push(cx.focus_handle().tab_index(0).tab_stop(true));
        }
        self.standalone_beads_focus_handles.truncate(self.standalone_badges.len());
        cx.notify();
    }

    /// Toggle the equalize icon (shown only when the active tab has 2+ panes).
    pub fn set_show_equalize(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_equalize == show {
            return;
        }
        self.show_equalize = show;
        cx.notify();
    }

    /// Make `index` the active tab and emit [`TitlebarEvent::SelectTab`].
    pub fn select(&mut self, index: usize, source: TabActivationSource, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            tab.shown = TabShown::from_active(i == index);
        }
        cx.emit(TitlebarEvent::SelectTab { index, source });
        cx.notify();
    }

    /// Remove `index` and emit [`TitlebarEvent::CloseTab`], keeping one tab active.
    pub fn close(&mut self, index: usize, cx: &mut Context<Self>) {
        let was_active = match self.tabs.get(index) {
            Some(tab) => tab.is_active(),
            None => return,
        };
        self.tabs.remove(index);
        self.tab_focus_handles.remove(index);
        self.tab_mirror_focus_handles.remove(index);
        self.tab_close_focus_handles.remove(index);
        self.beads_focus_handles.remove(index);
        if was_active && !self.tabs.is_empty() {
            let new_active = index.min(self.tabs.len() - 1);
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                tab.shown = TabShown::from_active(i == new_active);
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
        let (tab_x, _) = self.tab_geometry(source, origin_x);
        let grab_offset = cursor_x - tab_x;
        self.drag = Some(DragState {
            origin: source,
            source,
            origin_x,
            grab_offset,
            cursor_x,
            reordered: false,
            left_bar: false,
        });
        cx.notify();
    }

    /// The slot this drag began in and the slot the same tab occupies now.
    #[must_use]
    pub fn dragged_slot(&self) -> Option<(usize, usize, SessionId)> {
        let drag = self.drag?;
        let session_id = self.tabs.get(drag.source)?.session_id?;
        Some((drag.origin, drag.source, session_id))
    }

    /// Update the active drag to `cursor`, reordering when the dragged tab's
    /// centre crosses into a neighbour's slot. Emits
    /// [`TitlebarEvent::ReorderTab`] on each swap.
    ///
    /// Only a pointer inside the dragged tab's own group bar reorders. Once it
    /// leaves (down into a pane, or over another section's bar) the slot it
    /// last held freezes as the placeholder: the ghost is then aimed at
    /// another bar, and a source strip still shuffling under it read as the
    /// drag doing something other than what the pointer showed.
    pub fn update_drag(&mut self, cursor: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.drag else { return };
        let cursor_x = f32::from(cursor.x);
        drag.cursor_x = cursor_x;
        let (start, end) = self.group_of(drag.source);
        let (group_left, width) = self.tab_geometry(start, drag.origin_x);
        let group_right = group_left + px_units(end - start) * width;
        let in_bar = (0.0..=self.height).contains(&f32::from(cursor.y))
            && (group_left..=group_right).contains(&cursor_x);
        if !in_bar {
            drag.left_bar = true;
            self.drag = Some(drag);
            cx.notify();
            return;
        }
        // Resolve the slot from the dragged tab's centre, not the raw cursor:
        // a swap then always requires half a tab of overlap regardless of
        // where inside the tab the grab landed, so slots cannot thrash when
        // the pointer sits near a boundary.
        let center = cursor_x - drag.grab_offset + width / 2.0;
        let target = start
            + reorder_target_index(center, group_left, width, end - start, drag.source - start);
        if target != drag.source {
            self.move_tab(drag.source, target);
            cx.emit(TitlebarEvent::ReorderTab { from: drag.source, to: target });
            drag.source = target;
            drag.reordered = true;
        }
        self.drag = Some(drag);
        cx.notify();
    }

    /// End the active drag. `click_swallowed` reports whether GPUI's native
    /// drag engaged for this press (any pressed movement past its ~2 px
    /// threshold): engaging cancels the element's pending click, so an
    /// engaged press that never reordered and never left its bar was a
    /// jittered real-mouse click and must still select the pressed tab here.
    ///
    /// A drag that left the bar was aimed at another section. Selecting it
    /// here switched the source section onto the tab in the frame before the
    /// move took it away, so the source ended up showing a neighbour instead
    /// of the tab it had been showing.
    pub fn end_drag(&mut self, click_swallowed: bool, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if click_swallowed && !drag.reordered && !drag.left_bar {
                self.select(drag.source, TabActivationSource::Pointer, cx);
            }
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
        let mirror_focus = self.tab_mirror_focus_handles.remove(from);
        self.tab_mirror_focus_handles.insert(to, mirror_focus);
        let close_focus = self.tab_close_focus_handles.remove(from);
        self.tab_close_focus_handles.insert(to, close_focus);
        let beads_focus = self.beads_focus_handles.remove(from);
        self.beads_focus_handles.insert(to, beads_focus);
    }

    /// Left edge of the first tab: region edge plus an optional badge pill.
    ///
    /// Only the fallback for a strip no frame has measured yet; painted
    /// geometry comes from [`Self::tab_geometry`].
    fn tabs_origin_x(&self) -> f32 {
        self.tabs.first().map_or(0.0, |tab| {
            tab.group_region_x.unwrap_or(0.0)
                + TAB_RUN_INSET
                + tab.badge.as_ref().map_or(0.0, badge_width_px)
        })
    }

    /// Painted `(left, width, edge)` of every open tab, in window pixels, for
    /// the grid to bridge each one into the pane card below it.
    ///
    /// Measured geometry only: a tab no frame has painted yet has nothing to
    /// join to.
    #[must_use]
    pub fn open_tab_runs(&self) -> Vec<(f32, f32, Rgba)> {
        let measured = self.measured_tabs.borrow();
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let edge = open_tab_edge(tab, &self.colors)?;
                let (left, width) = measured.get(index).copied()?;
                (left.is_finite() && width > 0.0).then_some((left, width, edge))
            })
            .collect()
    }

    /// Whether a titlebar control owns keyboard focus. The shell uses this to
    /// avoid taking focus back to the terminal during an ordinary repaint.
    pub fn has_keyboard_focus(&self, window: &Window) -> bool {
        self.tab_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.tab_mirror_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.tab_close_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.equalize_focus_handle.is_focused(window)
            || self.beads_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.standalone_beads_focus_handles.iter().any(|handle| handle.is_focused(window))
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
            self.select(index, TabActivationSource::Keyboard, cx);
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

/// Pixel width of the badge, including the optional Beads icon target.
#[must_use]
pub fn badge_width_px(badge: &GroupBadge) -> f32 {
    px_units(badge.label.chars().count() + 2) * CHAR_WIDTH + if badge.beads { 42.0 } else { 16.0 }
}

/// The 7px square identity dot leading a workspace pill: the one place a
/// pill carries its workspace color now that every pill wears the shared
/// neutral chip fill (the layered chrome mock's `wdot`).
#[must_use]
pub fn workspace_pill_dot(accent: Rgba) -> AnyElement {
    div().flex_none().size(px(7.0)).rounded(px(2.0)).bg(accent).into_any_element()
}

/// Connected-node Beads mark shared by workspace badges in both tab bars.
#[must_use]
pub fn beads_graph_icon(color: Rgba) -> AnyElement {
    div()
        .relative()
        .size(px(16.0))
        .child(div().absolute().left(px(3.0)).top(px(4.0)).w(px(9.0)).h(px(1.0)).bg(color))
        .child(div().absolute().left(px(5.0)).top(px(8.0)).w(px(7.0)).h(px(1.0)).bg(color))
        .child(div().absolute().left(px(3.0)).top(px(2.0)).size(px(4.0)).rounded_full().bg(color))
        .child(div().absolute().right(px(1.0)).top(px(5.0)).size(px(4.0)).rounded_full().bg(color))
        .child(
            div().absolute().left(px(2.0)).bottom(px(1.0)).size(px(4.0)).rounded_full().bg(color),
        )
        .into_any_element()
}

/// Magnifier mark for the tab-mirror control: a ring and a stepped handle,
/// shared by both tab-bar renderers.
#[must_use]
pub fn magnifier_icon(color: Rgba) -> AnyElement {
    let step = |at: f32| div().absolute().left(px(at)).top(px(at)).size(px(2.0)).bg(color);
    div()
        .relative()
        .flex_none()
        .size(px(12.0))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .size(px(8.0))
                .rounded_full()
                .border_1()
                .border_color(color),
        )
        .child(step(7.0))
        .child(step(8.0))
        .child(step(9.0))
        .into_any_element()
}

/// One-column leading activity glyph shared by both tab-bar renderers.
#[must_use]
pub fn agent_active_glyph(color: Rgba) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(CHAR_WIDTH))
        .text_color(color)
        .child(AGENT_ACTIVE_GLYPH)
        .into_any_element()
}

/// AccessKit name for a tab, including the non-visual agent activity state.
#[must_use]
pub fn tab_accessible_label(tab: &TabData) -> String {
    if tab.agent_active { format!("{}, agent active", tab.title) } else { tab.title.clone() }
}

/// Half-open tab ranges split at workspace-region boundaries.
fn workspace_group_ranges(tabs: &[TabData]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    for (index, tab) in tabs.iter().enumerate() {
        match groups.last_mut() {
            Some((_, end)) if tab.group_region_x.is_none() => *end = index + 1,
            _ => groups.push((index, index + 1)),
        }
    }
    groups
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
    /// A resting tab draws a left hairline against a resting neighbour, so
    /// tab boundaries stay legible without giving every tab a standing frame.
    /// The hairline drops beside the active or hovered tab (whose fill marks
    /// its own edges) and at a workspace group's first tab, where the group
    /// pill is the boundary.
    fn tab_wants_left_separator(&self, index: usize, tab: &TabData) -> bool {
        if index == 0 || tab.is_active() || self.hovered_tab == Some(index) {
            return false;
        }
        if tab.group_region_x.is_some() {
            return false;
        }
        let Some(prev) = self.tabs.get(index - 1) else { return false };
        !prev.is_active() && self.hovered_tab != Some(index - 1)
    }

    /// The boundary mark a resting tab paints against a resting neighbour —
    /// an active or hovered tab's own fill already draws its edges. A short
    /// centred absolute child rather than a left border, so every mark is
    /// the same 13px regardless of tab height and layout never shifts by the
    /// border's pixel.
    fn tab_boundary_mark(&self, index: usize, tab: &TabData) -> Option<AnyElement> {
        self.tab_wants_left_separator(index, tab).then(|| {
            div()
                .absolute()
                .left_0()
                .top(gpui::relative(0.5))
                .mt(px(-6.5))
                .w(px(1.0))
                .h(px(13.0))
                .bg(self.colors.separator)
                .into_any_element()
        })
    }

    /// Base tab background before the attention flash is blended in.
    fn tab_base_bg(&self, tab: &TabData, is_hovered: bool) -> Rgba {
        if tab.is_active() {
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
            // Resting tabs paint no card, so this only serves as the flash
            // blend's base — the strip's own background.
            self.colors.bg
        }
    }

    fn render_tab_close(&self, close: TabClose<'_>, cx: &mut Context<Self>) -> AnyElement {
        let TabClose { index, id: tab_id, title, foreground: fg, visible, focused } = close;
        let Some(focus) = self.tab_close_focus_handles.get(index).cloned() else {
            return div().into_any_element();
        };
        div()
            .id((tab_id, "close"))
            .role(Role::Button)
            .aria_label(format!("Close {title}"))
            .track_focus(&focus)
            // The close glyph never shrinks: a narrowing tab compresses its
            // title instead, so the × stays whole.
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            // Collapse to zero width while hidden so a resting tab spends the
            // room on its title; the node stays mounted for focus stability.
            .w(if visible { px(14.0) } else { px(0.0) })
            .when(visible, gpui::Styled::ml_1)
            .opacity(if visible { 1.0 } else { 0.0 })
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

    fn tab_control_focus(&self, index: usize, window: &Window) -> TabControlFocus {
        let holds = |handles: &[FocusHandle]| {
            handles.get(index).is_some_and(|focus| focus.is_focused(window))
        };
        if holds(&self.tab_close_focus_handles) {
            TabControlFocus::Close
        } else if holds(&self.tab_mirror_focus_handles) {
            TabControlFocus::Mirror
        } else {
            TabControlFocus::None
        }
    }

    /// The hover-only magnifier beside a tab's close button, which opens the
    /// tab in a mirror window. Mounted like the close node, collapsed while
    /// hidden, so Tab traversal stays stable and focus can still reveal it.
    fn render_tab_mirror(&self, close: TabClose<'_>, cx: &mut Context<Self>) -> AnyElement {
        let TabClose { index, id: tab_id, title, foreground: fg, visible, focused } = close;
        let Some(focus) = self.tab_mirror_focus_handles.get(index).cloned() else {
            return div().into_any_element();
        };
        div()
            .id((tab_id, "mirror"))
            .role(Role::Button)
            .aria_label(format!("Open {title} in a mirror window"))
            .track_focus(&focus)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .w(if visible { px(14.0) } else { px(0.0) })
            .when(visible, gpui::Styled::ml_1)
            .opacity(if visible { 1.0 } else { 0.0 })
            .when(focused, |this| this.bg(self.colors.accent))
            .child(magnifier_icon(if focused { self.colors.active_text } else { fg }))
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(
                cx.listener(move |_, _, _window, ctx| ctx.emit(TitlebarEvent::MirrorTab(index))),
            )
            .on_key_down(cx.listener(move |_, event: &KeyDownEvent, window, ctx| {
                ctx.stop_propagation();
                if Self::focus_next_or_previous(event, window, ctx) {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    ctx.emit(TitlebarEvent::MirrorTab(index));
                }
            }))
            .into_any_element()
    }

    fn render_tab_children(&self, render: TabRender<'_>, cx: &mut Context<Self>) -> TabChildren {
        let TabRender { index, tab, foreground, id, focused, control_focus } = render;
        let close_focused = control_focus == TabControlFocus::Close;
        let mirror_focused = control_focus == TabControlFocus::Mirror;
        // No column budget here. The label is a flex child with `truncate`, so
        // it already ends in an ellipsis exactly where the tab runs out of
        // room; pre-cutting it to a fixed 22-column slot only made every title
        // shorter than the space it had, which is what it looked like.
        let display = tab.title.clone();
        let agent_indicator = tab.agent_active.then(|| agent_active_glyph(self.colors.accent));
        let ai_indicator = tab
            .ai_indicator
            .map(|color| div().size(px(6.0)).rounded_full().bg(color).mr_2().into_any_element());
        let suffix = tab.context_suffix.as_ref().map(|suffix| {
            div().text_color(suffix.color).child(suffix.text.clone()).into_any_element()
        });
        let visible =
            tab.is_focused() || self.hovered_tab == Some(index) || focused || close_focused;
        let mirror = Some(self.render_tab_mirror(
            TabClose {
                index,
                id: id.clone(),
                title: &tab.title,
                foreground,
                visible: self.hovered_tab == Some(index) || mirror_focused,
                focused: mirror_focused,
            },
            cx,
        ));
        let close = Some(self.render_tab_close(
            TabClose {
                index,
                id: id.clone(),
                title: &tab.title,
                foreground,
                visible,
                focused: close_focused,
            },
            cx,
        ));
        // The active mark, in the workspace's effective accent — the same
        // color the pill dot wears, so it stays a muted gray until a project
        // names the workspace and takes its palette color. Only the
        // window-focused region's shown tab wears it: other regions' shown
        // tabs keep the raised card (their open-tab marker) but go quiet, so
        // the mark answers "where is my keyboard" while the card answers
        // "which tab is open here".
        let mark =
            tab.is_focused().then(|| focus_mark(tab.group_accent.unwrap_or(self.colors.accent)));
        TabChildren { display, agent_indicator, ai_indicator, suffix, mirror, close, mark }
    }

    /// The Tab-key focus ring: an absolute child, not a border, because a
    /// border is a box inset that would shift the card and its outline 2px
    /// inside the slot the bridge below is painted for.
    fn keyboard_focus_ring(&self) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .rounded_t(px(6.0))
            .border_2()
            .border_color(self.colors.accent)
            .into_any_element()
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
        let fg = if tab.is_focused() { self.colors.active_text } else { self.colors.text };
        let tab_id = ElementId::from(tab.accessibility_id.clone());
        let Some(tab_focus) = self.tab_focus_handles.get(index).cloned() else {
            return div().into_any_element();
        };
        let dragging = self.drag.is_some_and(|drag| drag.source == index);
        if let Some(narrowed) = self.narrowed_tab_slot(index, tab, dragging) {
            return narrowed;
        }
        // The keyboard ring is for Tab-key navigation only. A click focuses
        // the tab too, for the one frame before the terminal takes focus
        // back, and a ring that flashed then read as the card's edge breaking.
        let focused =
            tab_focus.is_focused(focused_window) && focused_window.last_input_was_keyboard();
        let control_focus = self.tab_control_focus(index, focused_window);
        let children = self.render_tab_children(
            TabRender { index, tab, foreground: fg, id: &tab_id, focused, control_focus },
            cx,
        );

        let mut tab_el = div()
            .id(tab_id)
            .role(Role::Tab)
            .aria_label(tab_accessible_label(tab))
            .aria_selected(tab.is_active())
            .aria_position_in_set(index + 1)
            .aria_size_of_set(self.tabs.len())
            .track_focus(&tab_focus)
            .relative()
            .flex()
            .items_center()
            // A tab takes an equal share of the strip: with room to spare the
            // tabs grow into it rather than leaving the band empty, and in a
            // narrow group bar they shrink, truncating the flexed title while
            // the close button stays whole — instead of the bar's clip slicing
            // glyphs in half.
            .flex_grow(1.0)
            .flex_shrink_1()
            .flex_basis(px(TAB_WIDTH))
            .min_w(px(TAB_MIN_WIDTH))
            .max_w(px(TAB_MAX_WIDTH))
            .overflow_hidden()
            .h_full()
            .px_2()
            .text_color(fg)
            .text_xs();
        let show_card = !dragging && (tab.is_active() || is_hovered || tab.tab_flash.is_some());
        tab_el = tab_el
            .children(show_card.then(|| tab_card(bg, open_tab_edge(tab, &self.colors))))
            .children(dragging.then(|| drag_placeholder(&self.colors)))
            .children(self.tab_boundary_mark(index, tab));
        tab_el = tab_el.children(focused.then(|| self.keyboard_focus_ring()));
        tab_el = tab_el.child(self.tab_geometry_probe(index));
        let ghost = TabDragGhost {
            title: tab.title.clone(),
            colors: self.colors,
            width: self.tab_geometry(index, self.tabs_origin_x()).1,
            height: self.height,
            grab: Rc::default(),
        };
        // The slot in hand is an empty recess: its title travels with the ghost.
        if !dragging {
            tab_el = children.attach(tab_el);
        }
        let tab_el = tab_el
            .on_hover(cx.listener(move |this, hovered: &bool, _win, ctx| {
                this.hovered_tab = if *hovered { Some(index) } else { None };
                ctx.notify();
            }))
            .on_mouse_down(MouseButton::Left, Self::tab_press_listener(index, cx))
            // Registers [`TabDrag`] as GPUI's active drag once the pressed
            // pointer travels past the drag threshold; from then on the root's
            // `on_drag_move` receives every mouse move in the window.
            .on_drag(TabDrag(ghost), |drag, grab, _, cx| {
                drag.0.grab.set((f32::from(grab.x), f32::from(grab.y)));
                cx.new(|_| drag.0.clone())
            })
            .on_click(cx.listener(move |this, _, _window, ctx| {
                if !this.drag.as_ref().is_some_and(|drag| drag.reordered) {
                    this.select(index, TabActivationSource::Pointer, ctx);
                }
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, ctx| {
                ctx.stop_propagation();
                this.tab_key(index, event, window, ctx);
            }));
        tab_el.into_any_element()
    }

    /// The tab's slot while it is not fully open: closing because the tab
    /// is aimed at another bar or was released elsewhere, or reopening
    /// because the pointer came back. Its geometry probe is left out, so drag
    /// geometry keeps the last full-width measurement.
    fn narrowed_tab_slot(&self, index: usize, tab: &TabData, dragging: bool) -> Option<AnyElement> {
        let session_id = tab.session_id?;
        let open = self.drop_motion.slot(session_id, std::time::Instant::now());
        (open < 1.0).then(|| {
            let width = self.tab_geometry(index, self.tabs_origin_x()).1 * open;
            narrowed_slot(width, dragging.then_some(&self.colors))
        })
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

    /// Sorted left edges of every top-row workspace group, including empty
    /// regions that render only a standalone pill.
    fn workspace_region_edges(&self) -> Vec<f32> {
        let mut edges: Vec<f32> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.group_region_x)
            .chain(self.standalone_badges.iter().map(|badge| badge.region_x))
            .collect();
        edges.sort_by(f32::total_cmp);
        edges
    }

    /// Lower the strip into workspace-group elements.
    ///
    /// When every group has a region edge and those edges strictly
    /// ascend (side-by-side regions), each group is absolutely positioned at
    /// its region's edge and clipped to its region's width, so the strip
    /// aligns with the regions below it by construction — no estimated text
    /// widths. Otherwise (single workspace, or stacked regions sharing an
    /// edge) the groups flow left-to-right as one row.
    fn render_tabs(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tabs = self.tabs.clone();
        // Stale slots are refreshed by this frame's probes; a shorter strip
        // drops the tail so no removed tab's geometry outlives it.
        self.measured_tabs.borrow_mut().resize(tabs.len(), (f32::NAN, 0.0));
        let groups = workspace_group_ranges(&tabs);
        let edges: Vec<Option<f32>> = groups
            .iter()
            .map(|&(start, _)| tabs.get(start).and_then(|tab| tab.group_region_x))
            .collect();
        // Aligned placement needs every group to name a distinct region edge;
        // group order in the strip does not matter, since each group anchors
        // at its own edge independently. Stacked regions share an edge, so
        // duplicates fall back to one flowed row.
        let sorted_edges = self.workspace_region_edges();
        let aligned = edges.iter().all(Option::is_some)
            && sorted_edges.windows(2).all(|pair| matches!(pair, [a, b] if b > a));
        groups
            .iter()
            .enumerate()
            .map(|(group, &(start, end))| {
                // No hairline closes the group off: the strip is the bare
                // window ground, and region identity lives in the pill dot,
                // the focused tab's tick, and the pane focus ring below.
                let mut row = div().flex().flex_row().items_center().h_full().px(px(TAB_RUN_INSET));
                if let Some(badge) = tabs.get(start).and_then(|tab| tab.badge.as_ref()) {
                    row = row.child(self.render_group_badge(start, badge, cx));
                }
                let row = row.children(self.render_group_cells((start, end), &tabs, window, cx));
                if !aligned {
                    // The group bar takes the strip so its tabs have something
                    // to grow into. Without this the row sizes to its content
                    // and every tab stays at its basis, leaving the band empty
                    // to the right no matter how wide the window is.
                    return row.flex_1().min_w(px(0.0)).overflow_hidden().into_any_element();
                }
                let left = edges.get(group).copied().flatten().unwrap_or(0.0);
                let row = row.absolute().top_0().left(px(left)).overflow_hidden();
                // The bar fills its region: clip at the next region's edge,
                // whichever group owns it, and the rightmost bar runs to the
                // window edge.
                match sorted_edges.iter().find(|edge| **edge > left) {
                    Some(next) => row.w(px((next - left).max(0.0))).into_any_element(),
                    None => row.right_0().into_any_element(),
                }
            })
            .collect()
    }

    /// One group's tabs, each preceded by its landing gap while one is open
    /// or closing there, and the group's trailing gap after the last.
    fn render_group_cells(
        &self,
        (start, end): (usize, usize),
        tabs: &[TabData],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let now = std::time::Instant::now();
        // One tab's width of this group, so a gap is the slot a tab fills.
        let full =
            self.tab_geometry(start, self.tabs_origin_x()).1.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
        let gap = |slot: Option<GapSlot>| {
            let open = slot.map_or(0.0, |slot| self.drop_motion.gap(slot, now));
            (open > 0.0).then(|| drop_gap(full * open, &self.colors))
        };
        let mut cells = Vec::new();
        for index in start..end {
            let Some(tab) = tabs.get(index) else { continue };
            cells.extend(gap(tab.session_id.map(GapSlot::Before)));
            cells.push(self.render_tab(index, tab, window, cx));
        }
        let last =
            end.checked_sub(1).and_then(|last| tabs.get(last)).and_then(|tab| tab.session_id);
        cells.extend(gap(last.map(GapSlot::After)));
        cells
    }

    /// The workspace pill opening `index`'s group. Clicking it selects the
    /// group's first tab, which focuses that workspace's region.
    fn render_group_badge(
        &self,
        index: usize,
        badge: &GroupBadge,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The mock's uniform quiet chip: every pill shares one neutral fill,
        // and the workspace color lives in the leading square dot — or the
        // Beads icon standing in its place when a board is detected.
        let chip_bg = pill_fill(&self.colors);
        let label = div()
            .id(ElementId::from(format!("workspace-badge-{index}")))
            .role(Role::Button)
            .aria_label(format!("{} workspace; drag to rearrange", badge.label))
            .flex()
            .items_center()
            .px_2()
            .h_full()
            .text_color(self.colors.active_text)
            .text_xs()
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _win, ctx| {
                    // Do not stop propagation here: GPUI records the same
                    // press later in the bubble phase to arm `on_drag`.
                    this.workspace_drag_armed = true;
                    ctx.emit(TitlebarEvent::ArmWorkspaceDrag(
                        TitlebarWorkspaceDragSource::TabIndex(index),
                    ));
                }),
            )
            .on_drag(WorkspaceDragMarker, |_, _, _, cx| cx.new(|_| EmptyWorkspaceDragGhost))
            .on_click(cx.listener(move |this, _, _window, ctx| {
                this.select(index, TabActivationSource::Pointer, ctx);
            }))
            .hover(|style| style.rounded(px(4.0)).bg(self.colors.gradient_top))
            .child(badge.label.clone());
        // The pill shares the tab card's vertical rhythm — the same top gap,
        // flush to the band's bottom — so pills and tab cards sit on one
        // line. The horizontal padding keeps the dot, Beads icon, and label
        // off the pill's rounded edges.
        let mut row = div()
            .flex()
            .flex_none()
            .items_center()
            .mt(px(TAB_CARD_TOP_GAP))
            .h(px(self.height - TAB_CARD_TOP_GAP))
            .px(px(6.0))
            .rounded(px(6.0))
            .overflow_hidden()
            .bg(chip_bg);
        if badge.beads {
            let focus = self
                .beads_focus_handles
                .get(index)
                .cloned()
                .unwrap_or_else(|| self.focus_handle.clone());
            row = row.child(
                div()
                    .id(ElementId::from(format!("workspace-beads-{index}")))
                    .role(Role::Button)
                    .aria_label(format!("Open {} Beads board", badge.label))
                    .track_focus(&focus)
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(26.0))
                    .h_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(self.colors.gradient_top))
                    .on_hover(cx.listener(move |_this, hovered: &bool, _window, ctx| {
                        ctx.emit(TitlebarEvent::BeadsHover { index, hovered: *hovered });
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _window, ctx| {
                        ctx.stop_propagation();
                    })
                    .on_click(cx.listener(move |_this, _, window, ctx| {
                        window.focus(&focus, ctx);
                        ctx.emit(TitlebarEvent::ToggleBeadsBoard { index });
                    }))
                    // The workspace's own accent, so a Beads pill still
                    // carries its identity color where the dot would sit.
                    .child(beads_graph_icon(badge.accent)),
            );
        } else {
            row = row.child(workspace_pill_dot(badge.accent));
        }
        row.child(label).into_any_element()
    }

    /// Render pills for top-row workspace regions that have no tabs yet.
    fn render_standalone_badges(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let edges = self.workspace_region_edges();
        self.standalone_badges
            .iter()
            .enumerate()
            .map(|(index, badge)| {
                let next = edges.iter().copied().find(|edge| *edge > badge.region_x);
                self.render_standalone_badge(index, badge, next, cx)
            })
            .collect()
    }

    fn render_standalone_badge_label(
        &self,
        index: usize,
        standalone: &StandaloneBadge,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = standalone.workspace_id;
        div()
            .id(ElementId::from(format!("standalone-workspace-badge-{index}")))
            .role(Role::Button)
            .aria_label(format!("{} workspace; drag to rearrange", standalone.badge.label))
            .flex()
            .items_center()
            .px_2()
            .h_full()
            .text_color(self.colors.active_text)
            .text_xs()
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _win, ctx| {
                    // Do not stop propagation here: GPUI records the same
                    // press later in the bubble phase to arm `on_drag`.
                    this.workspace_drag_armed = true;
                    ctx.emit(TitlebarEvent::ArmWorkspaceDrag(
                        TitlebarWorkspaceDragSource::Workspace(workspace_id),
                    ));
                }),
            )
            .on_drag(WorkspaceDragMarker, |_, _, _, cx| cx.new(|_| EmptyWorkspaceDragGhost))
            .on_click(cx.listener(move |_this, _, _window, ctx| {
                ctx.emit(TitlebarEvent::FocusWorkspace(workspace_id));
            }))
            .hover(|style| style.rounded(px(4.0)).bg(self.colors.gradient_top))
            .child(standalone.badge.label.clone())
            .into_any_element()
    }

    fn render_standalone_badge(
        &self,
        index: usize,
        standalone: &StandaloneBadge,
        next_region_x: Option<f32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip_bg = pill_fill(&self.colors);
        let workspace_id = standalone.workspace_id;
        let label = self.render_standalone_badge_label(index, standalone, cx);
        // Same rhythm as the tab-run badge: top gap, bottom flush.
        let mut pill = div()
            .flex()
            .flex_none()
            .items_center()
            .mt(px(TAB_CARD_TOP_GAP))
            .h(px(self.height - TAB_CARD_TOP_GAP))
            .px(px(6.0))
            .rounded(px(6.0))
            .overflow_hidden()
            .bg(chip_bg);
        if standalone.badge.beads {
            let focus = self
                .standalone_beads_focus_handles
                .get(index)
                .cloned()
                .unwrap_or_else(|| self.focus_handle.clone());
            pill = pill.child(
                div()
                    .id(ElementId::from(format!("standalone-workspace-beads-{index}")))
                    .role(Role::Button)
                    .aria_label(format!("Open {} Beads board", standalone.badge.label))
                    .track_focus(&focus)
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(26.0))
                    .h_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(self.colors.gradient_top))
                    .on_hover(cx.listener(move |_this, hovered: &bool, _window, ctx| {
                        ctx.emit(TitlebarEvent::StandaloneBeadsHover {
                            workspace_id,
                            hovered: *hovered,
                        });
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _window, ctx| {
                        ctx.stop_propagation();
                    })
                    .on_click(cx.listener(move |_this, _, window, ctx| {
                        window.focus(&focus, ctx);
                        ctx.emit(TitlebarEvent::ToggleStandaloneBeadsBoard(workspace_id));
                    }))
                    .child(beads_graph_icon(standalone.badge.accent)),
            );
        } else {
            pill = pill.child(workspace_pill_dot(standalone.badge.accent));
        }
        let row = div()
            .absolute()
            .top_0()
            .left(px(standalone.region_x))
            .h_full()
            .px(px(TAB_RUN_INSET))
            .overflow_hidden();
        let row = match next_region_x {
            Some(next) => row.w(px((next - standalone.region_x).max(0.0))),
            None => row.right_0(),
        };
        row.child(pill.child(label)).into_any_element()
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
}

impl Render for TitlebarView {
    fn render(
        &mut self,
        focused_window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.advance_drop_motion(std::time::Instant::now(), focused_window, cx);
        let tabs = self.render_tabs(focused_window, cx);
        let standalone_badges = self.render_standalone_badges(cx);
        let equalize = self.render_equalize_button(focused_window, cx);

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
            .h(px(self.height))
            .bg(self.colors.bg)
            // Declared for Windows, where the platform consults the hit-test
            // areas. X11/Wayland ignore it, so the handlers below drive the move.
            .window_control_area(WindowControlArea::Drag)
            // Bubble-phase listeners run front-to-back, so a tab's own
            // `on_mouse_down` has already armed `drag` by the time this runs;
            // an active tab drag therefore never arms a window move.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _win, _ctx| {
                    this.move_arm = (this.drag.is_none() && !this.workspace_drag_armed)
                        .then_some(event.position);
                }),
            )
            // A press anywhere outside the titlebar ends any arm it left behind.
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _win, _ctx| {
                this.move_arm = None;
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, win, _ctx| {
                this.advance_move_arm(event, win);
            }))
            // Fires for every mouse move anywhere in the window while a
            // [`TabDrag`] is active, so the drag keeps tracking the cursor
            // after it leaves the titlebar band.
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _win, ctx| {
                this.update_drag(event.event.position, ctx);
            }))
            // `has_active_drag` is read before GPUI clears the drag (that
            // happens only after event dispatch), so it tells `end_drag`
            // whether this press's click was swallowed by the drag system.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _win, ctx| {
                    this.move_arm = None;
                    this.workspace_drag_armed = false;
                    let click_swallowed = ctx.has_active_drag();
                    this.end_drag(click_swallowed, ctx);
                }),
            )
            // A release outside the titlebar still ends the drag: the reorders
            // already committed while dragging stay, and no stale drag state
            // can pin the dragged tab off its slot.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _win, ctx| {
                    this.move_arm = None;
                    this.workspace_drag_armed = false;
                    let click_swallowed = ctx.has_active_drag();
                    this.end_drag(click_swallowed, ctx);
                }),
            )
            .child(
                div()
                    .id("terminal-tabs")
                    .role(Role::TabList)
                    .aria_label("Terminal tabs")
                    .relative()
                    .flex()
                    .items_center()
                    .h_full()
                    // Spans the whole band and clips, so a long strip slides
                    // under nothing: tabs cut off at the container edge.
                    .flex_1()
                    .overflow_hidden()
                    // Draggable backdrop under the groups — the old spacer,
                    // now covering every empty part of the strip.
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .window_control_area(WindowControlArea::Drag),
                    )
                    .children(tabs)
                    .children(standalone_badges),
            )
            .children(equalize)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use gpui::{AppContext as _, Entity, TestAppContext};
    use scribe_common::{ids::WorkspaceId, theme::minimal_dark};

    use super::{
        StandaloneBadge, TAB_RUN_INSET, TabActivationSource, TitlebarEvent, TitlebarView,
        badge_width_px, tab_accessible_label, workspace_group_ranges,
    };
    use crate::tab_bar::{GroupBadge, TabBarColors, TabData, TabShown};

    const TEST_TAB_BAR_HEIGHT: f32 = 36.0;

    /// A pointer at `x` inside the titlebar band.
    fn drag_point(x: f32) -> gpui::Point<gpui::Pixels> {
        gpui::point(gpui::px(x), gpui::px(TEST_TAB_BAR_HEIGHT / 2.0))
    }

    /// Create a titlebar seeded with `n` tabs (the first active) and a captured
    /// event log.
    fn titlebar_with_tabs(
        n: usize,
        cx: &mut TestAppContext,
    ) -> (Entity<TitlebarView>, Arc<Mutex<Vec<TitlebarEvent>>>) {
        let colors = TabBarColors::from(&minimal_dark().chrome);
        let bar = cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, TEST_TAB_BAR_HEIGHT, cx);
            let tabs = (0..n)
                .map(|i| {
                    let mut tab = TabData::new(format!("tab-{i}"));
                    tab.shown = TabShown::from_active(i == 0);
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

    #[test]
    fn unnamed_workspace_regions_keep_separate_tab_groups() {
        let mut tabs = vec![TabData::new("one"), TabData::new("two"), TabData::new("three")];
        tabs[0].group_region_x = Some(0.0);
        tabs[2].group_region_x = Some(320.0);

        assert!(tabs.iter().all(|tab| tab.badge.is_none()));
        assert_eq!(workspace_group_ranges(&tabs), vec![(0, 2), (2, 3)]);
        assert_eq!(
            tabs.iter().map(|tab| tab.title.as_str()).collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
    }

    #[gpui::test]
    fn top_region_badge_data_keeps_named_and_unnamed_pills(cx: &mut TestAppContext) {
        let colors = TabBarColors::from(&minimal_dark().chrome);
        let named = StandaloneBadge {
            workspace_id: WorkspaceId::new(),
            region_x: 0.0,
            badge: GroupBadge { label: "scribe".to_owned(), accent: colors.accent, beads: true },
        };
        let unnamed = StandaloneBadge {
            workspace_id: WorkspaceId::new(),
            region_x: 320.0,
            badge: GroupBadge {
                label: "workspace".to_owned(),
                accent: colors.separator,
                beads: false,
            },
        };
        let bar = cx.new(|cx| TitlebarView::new(colors, TEST_TAB_BAR_HEIGHT, cx));

        bar.update(cx, |bar, cx| {
            bar.set_standalone_badges(vec![named.clone(), unnamed.clone()], cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.standalone_badges, vec![named, unnamed]);
        });
    }

    #[gpui::test]
    fn zero_tab_top_region_keeps_standalone_badge(cx: &mut TestAppContext) {
        let colors = TabBarColors::from(&minimal_dark().chrome);
        let badge = StandaloneBadge {
            workspace_id: WorkspaceId::new(),
            region_x: 0.0,
            badge: GroupBadge {
                label: "workspace".to_owned(),
                accent: colors.separator,
                beads: false,
            },
        };
        let bar = cx.new(|cx| TitlebarView::new(colors, TEST_TAB_BAR_HEIGHT, cx));

        bar.update(cx, |bar, cx| bar.set_standalone_badges(vec![badge.clone()], cx));
        bar.read_with(cx, |bar, _| {
            assert!(bar.tabs.is_empty());
            assert_eq!(bar.standalone_badges, vec![badge]);
        });
    }

    #[gpui::test]
    fn live_height_updates_the_retained_view(cx: &mut TestAppContext) {
        let (bar, _) = titlebar_with_tabs(1, cx);
        bar.update(cx, |bar, cx| bar.set_height(80.0, cx));
        bar.read_with(cx, |bar, _| assert!((bar.height() - 80.0).abs() < f32::EPSILON));
    }

    #[test]
    fn agent_active_is_included_in_the_accessible_tab_label() {
        let mut tab = TabData::new("build");
        assert_eq!(tab_accessible_label(&tab), "build");
        tab.agent_active = true;
        assert_eq!(tab_accessible_label(&tab), "build, agent active");
    }

    #[gpui::test]
    fn agent_glyph_and_ai_dot_are_both_lowered_for_one_tab(cx: &mut TestAppContext) {
        let (bar, _) = titlebar_with_tabs(1, cx);
        bar.update(cx, |bar, cx| {
            let mut tab = bar.tabs[0].clone();
            tab.agent_active = true;
            tab.ai_indicator = Some(bar.colors.accent);
            let id = gpui::ElementId::from(tab.accessibility_id.clone());
            let children = bar.render_tab_children(
                super::TabRender {
                    index: 0,
                    tab: &tab,
                    foreground: bar.colors.text,
                    id: &id,
                    focused: false,
                    control_focus: super::TabControlFocus::None,
                },
                cx,
            );
            assert!(children.agent_indicator.is_some());
            assert!(children.ai_indicator.is_some());
        });
    }

    #[gpui::test]
    fn first_tab_origin_uses_region_edge_with_optional_badge(cx: &mut TestAppContext) {
        let (bar, _) = titlebar_with_tabs(1, cx);
        bar.update(cx, |bar, _| {
            bar.tabs[0].group_region_x = Some(24.0);
            let run = 24.0 + TAB_RUN_INSET;
            assert!((bar.tabs_origin_x() - run).abs() < f32::EPSILON);

            bar.tabs[0].badge = Some(GroupBadge {
                label: "scribe".to_owned(),
                accent: bar.colors.accent,
                beads: true,
            });
            let badge = bar.tabs[0].badge.as_ref().expect("badge");
            assert!((bar.tabs_origin_x() - run - badge_width_px(badge)).abs() < f32::EPSILON);
        });
    }

    // @lat: [[client#GPUI Titlebar#Selecting a tab activates it and emits]]
    #[gpui::test]
    fn select_activates_the_tab_and_emits(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| bar.select(2, TabActivationSource::Pointer, cx));
        bar.read_with(cx, |bar, _| {
            assert!(bar.tabs()[2].is_active());
            assert!(!bar.tabs()[0].is_active());
        });
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[TitlebarEvent::SelectTab { index: 2, source: TabActivationSource::Pointer }]
        );
    }

    #[gpui::test]
    fn keyboard_selection_emits_keyboard_source(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| bar.select(1, TabActivationSource::Keyboard, cx));
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[TitlebarEvent::SelectTab { index: 1, source: TabActivationSource::Keyboard }]
        );
    }

    // @lat: [[client#GPUI Titlebar#Closing a tab removes it and reactivates]]
    #[gpui::test]
    fn close_removes_and_keeps_one_active(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| bar.close(0, cx));
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs().len(), 2);
            assert!(bar.tabs().iter().filter(|t| t.is_active()).count() == 1);
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
            bar.update_drag(drag_point(super::TAB_WIDTH * 2.5), cx);
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs()[2].title, "tab-0");
        });
        assert!(log.lock().unwrap().iter().any(|e| matches!(e, TitlebarEvent::ReorderTab { .. })));
    }

    // @lat: [[client#GPUI Titlebar#Slot swaps track the dragged tab's centre]]
    #[gpui::test]
    fn slot_swaps_track_the_dragged_tabs_centre(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        // Grab tab 0 near its right edge: the cursor enters tab 1's slot
        // almost immediately, but the tab's centre has barely moved.
        bar.update(cx, |bar, cx| {
            bar.begin_drag(0, super::TAB_WIDTH - 6.0, 0.0, cx);
            bar.update_drag(drag_point(super::TAB_WIDTH + 40.0), cx);
        });
        bar.read_with(cx, |bar, _| assert_eq!(bar.tabs()[0].title, "tab-0"));
        assert!(log.lock().unwrap().is_empty());
        // Once the centre crosses into slot 1 (tab left past half a width),
        // the swap happens.
        bar.update(cx, |bar, cx| {
            bar.update_drag(drag_point(super::TAB_WIDTH * 1.5 - 5.0), cx);
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| assert_eq!(bar.tabs()[1].title, "tab-0"));
        assert_eq!(log.lock().unwrap().as_slice(), &[TitlebarEvent::ReorderTab { from: 0, to: 1 }]);
    }

    // @lat: [[client#GPUI Titlebar#A drag survives leaving the tab strip]]
    #[gpui::test]
    fn drag_survives_positions_outside_the_strip(cx: &mut TestAppContext) {
        let (bar, _) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| {
            bar.begin_drag(0, 0.0, 0.0, cx);
            // Way past the strip's right edge: the drag stays live and the
            // slot freezes where it was, since the pointer left the bar.
            bar.update_drag(drag_point(super::TAB_WIDTH * 40.0), cx);
            assert!(bar.drag.is_some_and(|drag| drag.source == 0));
            // Below the band likewise: a pointer over the panes reorders nothing.
            bar.update_drag(gpui::point(gpui::px(super::TAB_WIDTH * 2.5), gpui::px(200.0)), cx);
            assert!(bar.drag.is_some_and(|drag| drag.source == 0));
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| assert_eq!(bar.tabs()[0].title, "tab-0"));
    }

    // @lat: [[client#GPUI Titlebar#A drag reorders within its workspace group only]]
    #[gpui::test]
    fn drag_stays_inside_its_workspace_group(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(4, cx);
        // Two side-by-side top regions: tabs 0-1 and tabs 2-3.
        bar.update(cx, |bar, cx| {
            bar.tabs[0].group_region_x = Some(0.0);
            bar.tabs[2].group_region_x = Some(super::TAB_WIDTH * 2.0);
            bar.begin_drag(0, 0.0, 0.0, cx);
            // Inside the first group, past slot 1's centre: an ordinary swap.
            bar.update_drag(drag_point(super::TAB_WIDTH * 1.4), cx);
            assert!(bar.drag.is_some_and(|drag| drag.source == 1));
            // Far into the second group's run: the slot freezes at the end of
            // the first group, leaving the cross-region move to the shell's
            // release path.
            bar.update_drag(drag_point(super::TAB_WIDTH * 3.5), cx);
            assert!(bar.drag.is_some_and(|drag| drag.source == 1));
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs()[1].title, "tab-0");
            assert_eq!(bar.tabs()[2].title, "tab-2");
        });
        assert_eq!(log.lock().unwrap().as_slice(), &[TitlebarEvent::ReorderTab { from: 0, to: 1 }]);
    }

    // @lat: [[client#GPUI Titlebar#Release outside the strip commits the reorder]]
    #[gpui::test]
    fn release_outside_the_strip_commits_the_reorder(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| {
            bar.begin_drag(0, 0.0, 0.0, cx);
            // The pointer wanders below the titlebar band mid-drag...
            bar.update_drag(drag_point(super::TAB_WIDTH * 2.5), cx);
            // ...and the mouse-up lands there too (the root's `up_out` path).
            bar.end_drag(true, cx);
            assert!(bar.drag.is_none());
        });
        bar.read_with(cx, |bar, _| assert_eq!(bar.tabs()[2].title, "tab-0"));
        assert!(log.lock().unwrap().iter().any(|e| matches!(e, TitlebarEvent::ReorderTab { .. })));
    }

    // @lat: [[client#GPUI Titlebar#A drag arm without reordering still selects]]
    #[gpui::test]
    fn drag_arm_without_reorder_keeps_click_selection_live(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| {
            // A jittered real-mouse click: GPUI's native drag engages after a
            // couple of px of pressed movement and cancels the element's
            // pending click, so the release itself must select the pressed
            // tab (`click_swallowed = true`, no reorder).
            bar.begin_drag(1, super::TAB_WIDTH + 4.0, 0.0, cx);
            bar.update_drag(drag_point(super::TAB_WIDTH + 20.0), cx);
            assert!(bar.drag.as_ref().is_some_and(|drag| !drag.reordered));
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| {
            assert!(bar.tabs()[1].is_active());
            assert!(!bar.tabs()[0].is_active());
        });
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[TitlebarEvent::SelectTab { index: 1, source: TabActivationSource::Pointer }]
        );
    }

    // @lat: [[client#GPUI Titlebar#A drag that left its bar does not select]]
    #[gpui::test]
    fn drag_that_left_its_bar_does_not_select(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| {
            // Grab tab 1 and carry it below the band, as a drag aimed at
            // another section does; it never reorders.
            bar.begin_drag(1, super::TAB_WIDTH + 4.0, 0.0, cx);
            bar.update_drag(gpui::point(gpui::px(super::TAB_WIDTH + 4.0), gpui::px(300.0)), cx);
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| assert!(bar.tabs()[0].is_active()));
        assert!(log.lock().unwrap().is_empty());
    }

    // @lat: [[client#GPUI Titlebar#Out-of-range interactions are no-ops]]
    #[gpui::test]
    fn out_of_range_interactions_are_noops(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(2, cx);
        bar.update(cx, |bar, cx| {
            bar.select(9, TabActivationSource::Keyboard, cx);
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
            bar.update_drag(drag_point(super::TAB_WIDTH * 2.5), cx);
            bar.end_drag(true, cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs()[2].accessibility_id, original_ids[0]);
        });
    }
}
