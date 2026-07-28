//! Right-click context menu overlay for the GPUI client rebuild.
//!
//! The winit client built the menu's item list from the right-click context
//! (selection state, a hovered heuristic URL, an OSC 8 URI, a file path, and any
//! smart-selection actions), painted it as GPU quads, and hit-tested
//! clicks against cached item rects. This port keeps the pure item-assembly logic
//! — the fixed copy/paste/select-all head, the OSC 8 "Open URL" precedence and
//! "Copy hyperlink address" entry (spec 009 FR-003 / FR-007), the appended file
//! and smart-selection entries — in [`build_menu_items`], and lowers the paint
//! and hit-testing onto a GPUI [`ContextMenuView`] entity with a rounded, shadowed
//! box, per-item hover/pressed states, and greyed-out disabled rows.

use gpui::{Context, EventEmitter, FocusHandle, MouseButton, Point, Rgba, div, prelude::*, px};
use scribe_common::config::SmartSelectionActionKind;
use scribe_common::theme::ChromeColors;

use crate::smart_selection::ResolvedSmartSelectionAction;
use crate::tab_bar::srgba;

/// Action triggered by selecting a context menu item. Ported verbatim from the
/// winit `ContextMenuAction` so the shell dispatcher keeps identical routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMenuAction {
    /// Copy the current selection to the clipboard.
    Copy,
    /// Paste from the clipboard.
    Paste,
    /// Select all text in the focused pane.
    SelectAll,
    /// Open a heuristic-detected URL (routed through the existing allowlist).
    OpenUrl(String),
    /// Open an OSC 8 URI (spec 009 FR-003 / FR-015), routed through the OSC 8
    /// scheme-allowlist gate. Distinct from [`Self::OpenUrl`] so the dispatcher
    /// preserves today's silent-drop behaviour for non-allowlisted heuristic
    /// schemes.
    OpenOsc8Url(String),
    /// Open a file path.
    OpenFile(String),
    /// Run a shell command from a smart-selection action.
    RunCommand(String),
    /// Start a background coprocess command from a smart-selection action.
    RunCoprocess(String),
    /// Send text to the focused pane as typed input.
    SendText(String),
    /// Run a shell command in a newly created terminal tab.
    RunCommandInWindow(String),
    /// Copy specific text to the clipboard.
    CopyText(String),
    /// Copy an OSC 8 hyperlink address verbatim to the clipboard (spec 009
    /// FR-007).
    CopyHyperlinkAddress(String),
}

impl ContextMenuAction {
    /// Whether this action belongs to the "open/run" group that is visually
    /// separated from the copy/paste head by a divider. Mirrors the winit
    /// renderer's separator gate.
    #[must_use]
    fn is_open_group(&self) -> bool {
        !matches!(self, Self::Copy | Self::Paste | Self::SelectAll)
    }
}

/// A single item in the context menu.
#[derive(Clone, Debug)]
pub struct MenuItem {
    /// The row label.
    pub label: String,
    /// The action dispatched when the row is clicked.
    pub action: ContextMenuAction,
    /// If `false`, the item is greyed out and not clickable.
    pub enabled: bool,
}

/// The right-click context that determines which menu items are shown.
#[derive(Clone, Debug, Default)]
pub struct ContextMenuRequest {
    /// Whether a selection exists (enables Copy).
    pub has_selection: bool,
    /// A heuristic-detected URL under the cursor, if any.
    pub url: Option<String>,
    /// A file path under the cursor, if any.
    pub file_path: Option<String>,
    /// Smart-selection action items resolved for the cursor's logical line.
    pub smart_actions: Vec<MenuItem>,
    /// An OSC 8 URI carried by the cell under the right-click target (spec 009).
    /// When `Some`, it takes precedence over `url` for "Open URL" and appends a
    /// "Copy hyperlink address" entry.
    pub osc8_uri: Option<String>,
}

/// Assemble the ordered menu items for a right-click [`ContextMenuRequest`].
///
/// Ported verbatim from the winit `ContextMenu::new` item assembly: the fixed
/// Copy / Paste / Select All head (Copy enabled only with a selection), then the
/// OSC-8-precedence "Open URL", the file entry, the OSC 8 "Copy hyperlink
/// address" entry, and finally the smart-selection actions.
#[must_use]
pub fn build_menu_items(request: ContextMenuRequest) -> Vec<MenuItem> {
    let ContextMenuRequest { has_selection, url, file_path, smart_actions, osc8_uri } = request;
    let mut items = vec![
        MenuItem { label: "Copy".into(), action: ContextMenuAction::Copy, enabled: has_selection },
        MenuItem { label: "Paste".into(), action: ContextMenuAction::Paste, enabled: true },
        MenuItem {
            label: "Select All".into(),
            action: ContextMenuAction::SelectAll,
            enabled: true,
        },
    ];

    // FR-003 precedence: an OSC 8 URI carries the "Open URL" item verbatim via
    // the dedicated variant so the dispatcher routes it through the OSC 8
    // scheme-allowlist gate; heuristic URLs keep the silent-drop path.
    if let Some(uri) = osc8_uri.clone() {
        items.push(MenuItem {
            label: "Open URL".into(),
            action: ContextMenuAction::OpenOsc8Url(uri),
            enabled: true,
        });
    } else if let Some(url) = url {
        items.push(MenuItem {
            label: "Open URL".into(),
            action: ContextMenuAction::OpenUrl(url),
            enabled: true,
        });
    }

    if let Some(path) = file_path {
        items.push(MenuItem {
            label: "Open File".into(),
            action: ContextMenuAction::OpenFile(path),
            enabled: true,
        });
    }

    // FR-007: surface the OSC 8 URI as a dedicated copy entry after "Open File".
    if let Some(uri) = osc8_uri {
        items.push(MenuItem {
            label: "Copy hyperlink address".into(),
            action: ContextMenuAction::CopyHyperlinkAddress(uri),
            enabled: true,
        });
    }

    items.extend(smart_actions);
    items
}

/// Map a smart-selection action kind + parameter onto the matching context-menu
/// action. Ported verbatim from the winit `smart_selection_context_action`.
#[must_use]
pub fn smart_selection_context_action(
    kind: SmartSelectionActionKind,
    parameter: String,
) -> ContextMenuAction {
    match kind {
        SmartSelectionActionKind::OpenFile => ContextMenuAction::OpenFile(parameter),
        SmartSelectionActionKind::OpenUrl => ContextMenuAction::OpenUrl(parameter),
        SmartSelectionActionKind::RunCommand => ContextMenuAction::RunCommand(parameter),
        SmartSelectionActionKind::RunCoprocess => ContextMenuAction::RunCoprocess(parameter),
        SmartSelectionActionKind::SendText => ContextMenuAction::SendText(parameter),
        SmartSelectionActionKind::RunCommandInWindow => {
            ContextMenuAction::RunCommandInWindow(parameter)
        }
        SmartSelectionActionKind::Copy => ContextMenuAction::CopyText(parameter),
    }
}

/// Build a menu item from a resolved smart-selection action, dropping actions
/// whose expanded parameter is empty. Ported from the winit
/// `smart_selection_menu_item`.
#[must_use]
pub fn smart_selection_menu_item(action: ResolvedSmartSelectionAction) -> Option<MenuItem> {
    if action.parameter.is_empty() {
        return None;
    }
    Some(MenuItem {
        label: action.label,
        action: smart_selection_context_action(action.kind, action.parameter),
        enabled: true,
    })
}

/// Events the context menu emits for the shell to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMenuEvent {
    /// An enabled item was clicked; dispatch its action.
    Selected(ContextMenuAction),
    /// The menu was dismissed (Escape, or a click on the backdrop).
    Dismissed,
}

/// Resolved GPUI colours for the context menu box.
#[derive(Clone, Copy)]
pub struct ContextMenuColors {
    /// Menu box background.
    pub bg: Rgba,
    /// 1px border colour.
    pub border: Rgba,
    /// Divider colour between the copy head and the open/run group.
    pub separator: Rgba,
    /// Enabled item text.
    pub item_fg: Rgba,
    /// Disabled item text (greyed out).
    pub disabled_fg: Rgba,
    /// Hovered-row background.
    pub hover_bg: Rgba,
    /// Pressed-row background.
    pub pressed_bg: Rgba,
}

impl From<&ChromeColors> for ContextMenuColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            bg: lighten(srgba(chrome.tab_bar_bg), 0.04),
            border: with_alpha(srgba(chrome.tab_text), 0.20),
            separator: with_alpha(srgba(chrome.tab_text), 0.12),
            item_fg: srgba(chrome.tab_text_active),
            disabled_fg: with_alpha(srgba(chrome.tab_text), 0.35),
            hover_bg: with_alpha(srgba(chrome.tab_text), 0.12),
            pressed_bg: with_alpha(srgba(chrome.tab_text), 0.20),
        }
    }
}

/// The right-click context menu view. Positioned absolutely at the click point
/// inside the overlay layer; clicks on enabled rows emit
/// [`ContextMenuEvent::Selected`], Escape or a backdrop click emits
/// [`ContextMenuEvent::Dismissed`].
pub struct ContextMenuView {
    colors: ContextMenuColors,
    items: Vec<MenuItem>,
    /// Top-left corner of the menu box, in window pixels.
    position: Point<gpui::Pixels>,
    focus_handle: FocusHandle,
}

impl EventEmitter<ContextMenuEvent> for ContextMenuView {}

impl ContextMenuView {
    /// Build a context menu at `position` with the items assembled for `request`.
    pub fn new(
        colors: ContextMenuColors,
        request: ContextMenuRequest,
        position: Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self { colors, items: build_menu_items(request), position, focus_handle: cx.focus_handle() }
    }

    /// Borrow the assembled menu items (test/inspection surface).
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// Dispatch the item at `index` if it is enabled, emitting
    /// [`ContextMenuEvent::Selected`]. Out-of-range or disabled indices are
    /// no-ops.
    pub fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else { return };
        if !item.enabled {
            return;
        }
        cx.emit(ContextMenuEvent::Selected(item.action.clone()));
    }

    /// Dismiss the menu, clearing its rows and emitting
    /// [`ContextMenuEvent::Dismissed`].
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        cx.emit(ContextMenuEvent::Dismissed);
    }

    fn render_item(
        colors: ContextMenuColors,
        index: usize,
        item: &MenuItem,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fg = if item.enabled { colors.item_fg } else { colors.disabled_fg };
        let mut row = div()
            .id(("ctx-item", index))
            .w_full()
            .px_3()
            .py_1()
            .text_sm()
            .text_color(fg)
            .child(item.label.clone());
        if item.enabled {
            row = row
                .hover(move |s| s.bg(colors.hover_bg))
                .active(move |s| s.bg(colors.pressed_bg))
                .on_click(cx.listener(move |this, _, _win, ctx| this.activate(index, ctx)));
        }
        row.into_any_element()
    }
}

impl Render for ContextMenuView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let mut rows = Vec::with_capacity(self.items.len() * 2);
        let mut prev_open_group = false;
        let item_data: Vec<(usize, MenuItem)> = self.items.iter().cloned().enumerate().collect();
        for (index, item) in &item_data {
            // Insert a divider ahead of the first open/run-group entry, mirroring
            // the winit separator between the copy head and the action group.
            if item.action.is_open_group() && !prev_open_group {
                rows.push(div().mx_2().my_1().h(px(1.0)).bg(colors.separator).into_any_element());
                prev_open_group = true;
            }
            rows.push(Self::render_item(colors, *index, item, cx));
        }

        // Full-window backdrop: a click anywhere outside the box dismisses.
        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _win, ctx| this.dismiss(ctx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _win, ctx| this.dismiss(ctx)))
            .child(
                div()
                    .absolute()
                    .left(self.position.x)
                    .top(self.position.y)
                    .min_w(px(160.0))
                    .flex()
                    .flex_col()
                    .py_1()
                    .bg(colors.bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .shadow_lg()
                    // Swallow clicks inside the box so they do not hit the backdrop.
                    .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                    .children(rows),
            )
    }
}

/// Lighten a GPUI colour by adding `amount` to each RGB channel, clamped to 1.0.
fn lighten(color: Rgba, amount: f32) -> Rgba {
    Rgba {
        r: (color.r + amount).min(1.0),
        g: (color.g + amount).min(1.0),
        b: (color.b + amount).min(1.0),
        a: color.a,
    }
}

/// Return `color` with a replaced alpha channel.
fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

#[cfg(test)]
mod tests;
