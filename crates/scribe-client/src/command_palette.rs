//! Command palette overlay for the GPUI client rebuild.
//!
//! The winit client split the palette across two files: a tiny `CommandPalette`
//! state struct (query, selection, open flag) plus a GPU painter, and
//! the entry/action machinery in `main.rs` — the base action list, the profile
//! rows, the conditional "update" row, and `execute_automation_action` routing.
//! This port folds all of that into one GPUI [`CommandPaletteView`] entity: the
//! pure entry assembly ([`base_entries`], [`profile_entries`], [`build_entries`])
//! and query filter ([`filter_entries`]) stay testable, while the box is drawn
//! with GPUI elements — rounded corners, a drop shadow, an input row, and
//! hover/selected item rows — instead of hand-placed quads. Confirming a row
//! emits its [`PaletteAction`] for the shell to route.

use gpui::{Context, EventEmitter, FocusHandle, Rgba, div, prelude::*, px};
use scribe_common::protocol::AutomationAction;
use scribe_common::theme::ChromeColors;

use crate::tab_bar::srgba;

/// Maximum number of filtered rows the palette shows at once, mirroring the winit
/// overlay's item cap.
pub const MAX_VISIBLE_ITEMS: usize = 8;

/// What a command-palette row does when confirmed. Most rows dispatch a shared
/// [`AutomationAction`]; feature 013 adds a client-local action that opens the
/// remote-connect picker without touching the wire protocol. Ported verbatim from
/// the winit `PaletteAction`.
#[derive(Clone, Debug)]
pub enum PaletteAction {
    /// Dispatch a shared automation action (the common case).
    Automation(AutomationAction),
    /// Open the feature-013 remote-connect picker (client-local, off-wire).
    OpenRemoteConnect,
    /// Move the focused workspace into a fresh window.
    MoveWorkspaceToNewWindow,
    /// Move the focused workspace to its nearest left neighbour.
    MoveWorkspaceLeft,
    /// Move the focused workspace to its nearest right neighbour.
    MoveWorkspaceRight,
    /// Move the focused workspace to its nearest upper neighbour.
    MoveWorkspaceUp,
    /// Move the focused workspace to its nearest lower neighbour.
    MoveWorkspaceDown,
}

// `AutomationAction` (frozen scribe-common) derives neither `PartialEq` nor
// `Eq`, so compare it structurally through its derived `Debug` form — enough for
// these value-like, string/enum-only variants and the event assertions.
impl PartialEq for PaletteAction {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::OpenRemoteConnect, Self::OpenRemoteConnect)
            | (Self::MoveWorkspaceToNewWindow, Self::MoveWorkspaceToNewWindow)
            | (Self::MoveWorkspaceLeft, Self::MoveWorkspaceLeft)
            | (Self::MoveWorkspaceRight, Self::MoveWorkspaceRight)
            | (Self::MoveWorkspaceUp, Self::MoveWorkspaceUp)
            | (Self::MoveWorkspaceDown, Self::MoveWorkspaceDown) => true,
            (Self::Automation(a), Self::Automation(b)) => format!("{a:?}") == format!("{b:?}"),
            _ => false,
        }
    }
}

impl Eq for PaletteAction {}

/// A single command-palette row: its display label and the action it dispatches.
#[derive(Clone, Debug)]
pub struct CommandPaletteEntry {
    /// The row label shown in the list and matched against the query.
    pub label: String,
    /// The action confirmed when the row is selected.
    pub action: PaletteAction,
}

impl CommandPaletteEntry {
    /// Build an entry that dispatches a shared [`AutomationAction`].
    #[must_use]
    pub fn automation(label: impl Into<String>, action: AutomationAction) -> Self {
        Self { label: label.into(), action: PaletteAction::Automation(action) }
    }
}

/// The fixed base command-palette rows, in display order. Ported verbatim from
/// the winit `base_command_palette_entries`, including the feature-013
/// client-local "Connect to remote machine…" row.
#[must_use]
pub fn base_entries() -> Vec<CommandPaletteEntry> {
    vec![
        CommandPaletteEntry::automation("Open Settings", AutomationAction::OpenSettings),
        CommandPaletteEntry::automation("Find in Scrollback", AutomationAction::OpenFind),
        CommandPaletteEntry::automation("New Tab", AutomationAction::NewTab),
        CommandPaletteEntry::automation("New Claude Tab", AutomationAction::NewClaudeTab),
        CommandPaletteEntry::automation("Resume Claude Tab", AutomationAction::NewClaudeResumeTab),
        CommandPaletteEntry::automation("New Codex Tab", AutomationAction::NewCodexTab),
        CommandPaletteEntry::automation("Resume Codex Tab", AutomationAction::NewCodexResumeTab),
        CommandPaletteEntry::automation("Split Pane Vertical", AutomationAction::SplitVertical),
        CommandPaletteEntry::automation("Split Pane Horizontal", AutomationAction::SplitHorizontal),
        CommandPaletteEntry::automation("Close Pane", AutomationAction::ClosePane),
        CommandPaletteEntry::automation("Close Tab", AutomationAction::CloseTab),
        CommandPaletteEntry::automation("New Window", AutomationAction::NewWindow),
        CommandPaletteEntry {
            label: "Move workspace to new window".into(),
            action: PaletteAction::MoveWorkspaceToNewWindow,
        },
        CommandPaletteEntry {
            label: "Move workspace left".into(),
            action: PaletteAction::MoveWorkspaceLeft,
        },
        CommandPaletteEntry {
            label: "Move workspace right".into(),
            action: PaletteAction::MoveWorkspaceRight,
        },
        CommandPaletteEntry {
            label: "Move workspace up".into(),
            action: PaletteAction::MoveWorkspaceUp,
        },
        CommandPaletteEntry {
            label: "Move workspace down".into(),
            action: PaletteAction::MoveWorkspaceDown,
        },
        CommandPaletteEntry {
            label: "Connect to remote machine…".into(),
            action: PaletteAction::OpenRemoteConnect,
        },
    ]
}

/// The "Switch Profile" rows for `profile_names`, tagging the active one. Ported
/// from the winit `profile_command_palette_entries` with the global profile
/// lookup lifted to the caller so the assembly stays pure and testable.
#[must_use]
pub fn profile_entries(
    profile_names: &[String],
    active_profile: Option<&str>,
) -> Vec<CommandPaletteEntry> {
    profile_names
        .iter()
        .map(|name| {
            let mut label = format!("Switch Profile: {name}");
            if active_profile == Some(name.as_str()) {
                label.push_str(" (active)");
            }
            CommandPaletteEntry::automation(
                label,
                AutomationAction::SwitchProfile { name: name.clone() },
            )
        })
        .collect()
}

/// Assemble the full entry list: the base rows, then the conditional "Update
/// Scribe to v{version}" row when an update is available, then the profile rows.
/// Ported from the winit `command_palette_entries`.
#[must_use]
pub fn build_entries(
    update_available: Option<&str>,
    profile_names: &[String],
    active_profile: Option<&str>,
) -> Vec<CommandPaletteEntry> {
    let mut entries = base_entries();
    if let Some(version) = update_available {
        entries.push(CommandPaletteEntry::automation(
            format!("Update Scribe to v{version}"),
            AutomationAction::OpenUpdateDialog,
        ));
    }
    entries.extend(profile_entries(profile_names, active_profile));
    entries
}

/// Filter `entries` by a case-insensitive substring match on the trimmed query.
/// An empty query keeps every entry. Ported from the winit
/// `refresh_command_palette_items` filter.
#[must_use]
pub fn filter_entries(entries: &[CommandPaletteEntry], query: &str) -> Vec<CommandPaletteEntry> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return entries.to_vec();
    }
    entries.iter().filter(|e| e.label.to_lowercase().contains(&needle)).cloned().collect()
}

/// Events the command palette emits for the shell to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandPaletteEvent {
    /// A row was confirmed; route its [`PaletteAction`].
    Execute(PaletteAction),
    /// The palette was dismissed (Escape or backdrop click) without a choice.
    Dismissed,
}

/// Resolved GPUI colours for the palette box.
#[derive(Clone, Copy)]
pub struct CommandPaletteColors {
    /// Box background.
    pub bg: Rgba,
    /// Input field background.
    pub input_bg: Rgba,
    /// Border / accent colour.
    pub border: Rgba,
    /// Header text.
    pub header_fg: Rgba,
    /// Query text.
    pub query_fg: Rgba,
    /// Placeholder text (dimmed).
    pub placeholder_fg: Rgba,
    /// Item text.
    pub item_fg: Rgba,
    /// Selected-row background.
    pub selection_bg: Rgba,
    /// Selected-row text.
    pub selection_fg: Rgba,
    /// Hovered-row background.
    pub hover_bg: Rgba,
}

impl From<&ChromeColors> for CommandPaletteColors {
    fn from(chrome: &ChromeColors) -> Self {
        let mut bg = srgba(chrome.tab_bar_active_bg);
        bg.a = 0.96;
        let mut input_bg = srgba(chrome.status_bar_bg);
        input_bg.a = 0.98;
        let query_fg = srgba(chrome.status_bar_text);
        Self {
            bg,
            input_bg,
            border: srgba(chrome.accent),
            header_fg: srgba(chrome.tab_text_active),
            query_fg,
            placeholder_fg: with_alpha(query_fg, query_fg.a * 0.7),
            item_fg: srgba(chrome.tab_text_active),
            selection_bg: {
                let mut c = srgba(chrome.status_bar_bg);
                c.a = 1.0;
                c
            },
            selection_fg: srgba(chrome.tab_text_active),
            hover_bg: with_alpha(srgba(chrome.tab_text), 0.10),
        }
    }
}

/// The command palette view: an entry list, a live query filter, a highlighted
/// selection, and keyboard/click confirmation. Ported from the winit
/// `CommandPalette` state plus the `main.rs` entry/routing machinery.
pub struct CommandPaletteView {
    colors: CommandPaletteColors,
    /// The full, unfiltered entry list (rebuilt when the palette opens).
    entries: Vec<CommandPaletteEntry>,
    /// The current filter query.
    query: String,
    /// Index of the highlighted row within the filtered list.
    selected: usize,
    focus_handle: FocusHandle,
}

impl EventEmitter<CommandPaletteEvent> for CommandPaletteView {}

impl CommandPaletteView {
    /// Build a palette over `entries` with an empty query.
    pub fn new(
        colors: CommandPaletteColors,
        entries: Vec<CommandPaletteEntry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self { colors, entries, query: String::new(), selected: 0, focus_handle: cx.focus_handle() }
    }

    /// Replace the entry list and reset the query/selection (called on open).
    pub fn set_entries(&mut self, entries: Vec<CommandPaletteEntry>, cx: &mut Context<Self>) {
        self.entries = entries;
        self.query.clear();
        self.selected = 0;
        cx.notify();
    }

    /// The current query string.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The rows matching the current query (the visible list).
    #[must_use]
    pub fn filtered(&self) -> Vec<CommandPaletteEntry> {
        filter_entries(&self.entries, &self.query)
    }

    /// The highlighted index within the filtered list.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Append a typed character to the query and reset the selection.
    pub fn push_char(&mut self, c: char, cx: &mut Context<Self>) {
        self.query.push(c);
        self.selected = 0;
        cx.notify();
    }

    /// Append pasted text, dropping control characters so a multi-line clipboard
    /// payload collapses into the filter string. Ported from the winit
    /// `push_str`.
    pub fn push_str(&mut self, s: &str, cx: &mut Context<Self>) {
        self.query.extend(s.chars().filter(|c| !c.is_control()));
        self.selected = 0;
        cx.notify();
    }

    /// Remove the last query character and reset the selection.
    pub fn pop_char(&mut self, cx: &mut Context<Self>) {
        self.query.pop();
        self.selected = 0;
        cx.notify();
    }

    /// Move the selection to the next row (wrapping), within the filtered list.
    pub fn next_item(&mut self, cx: &mut Context<Self>) {
        let count = self.filtered().len();
        self.selected = if count == 0 { 0 } else { (self.selected + 1) % count };
        cx.notify();
    }

    /// Move the selection to the previous row (wrapping), within the filtered
    /// list.
    pub fn prev_item(&mut self, cx: &mut Context<Self>) {
        let count = self.filtered().len();
        self.selected = if count == 0 { 0 } else { (self.selected + count - 1) % count };
        cx.notify();
    }

    /// Confirm the highlighted row, emitting [`CommandPaletteEvent::Execute`].
    /// A no-op when the filtered list is empty.
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        let filtered = self.filtered();
        if let Some(entry) = filtered.get(self.selected) {
            cx.emit(CommandPaletteEvent::Execute(entry.action.clone()));
        }
    }

    /// Dismiss the palette without a choice, clearing the query and selection so
    /// a later reopen starts fresh.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.query.clear();
        self.selected = 0;
        cx.emit(CommandPaletteEvent::Dismissed);
    }

    fn render_item(
        colors: CommandPaletteColors,
        index: usize,
        entry: &CommandPaletteEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fg = if selected { colors.selection_fg } else { colors.item_fg };
        let mut row = div()
            .id(("palette-item", index))
            .w_full()
            .px_3()
            .py_1()
            .rounded_sm()
            .text_sm()
            .text_color(fg)
            .child(entry.label.clone());
        if selected {
            row = row.bg(colors.selection_bg);
        }
        row.hover(move |s| s.bg(colors.hover_bg))
            .active(move |s| s.bg(colors.selection_bg))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.selected = index;
                this.confirm(ctx);
            }))
            .into_any_element()
    }
}

impl Render for CommandPaletteView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let filtered = self.filtered();
        let query_empty = self.query.trim().is_empty();

        let mut rows = Vec::new();
        if filtered.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(colors.placeholder_fg)
                    .child("No matching commands")
                    .into_any_element(),
            );
        } else {
            let selected = self.selected;
            for (index, entry) in filtered.iter().take(MAX_VISIBLE_ITEMS).enumerate() {
                rows.push(Self::render_item(colors, index, entry, index == selected, cx));
            }
        }

        let query_text = if query_empty {
            "Type a command or profile name".to_owned()
        } else {
            self.query.clone()
        };
        let query_color = if query_empty { colors.placeholder_fg } else { colors.query_fg };

        // Backdrop: a click outside the box dismisses.
        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _win, ctx| this.dismiss(ctx)),
            )
            .child(
                div()
                    .mt(px(96.0))
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .bg(colors.bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_lg()
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                    .child(
                        div()
                            .px_3()
                            .pt_2()
                            .text_xs()
                            .text_color(colors.header_fg)
                            .child("Command Palette"),
                    )
                    .child(
                        div()
                            .mx_2()
                            .mt_1()
                            .mb_2()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .bg(colors.input_bg)
                            .rounded_md()
                            .child(div().text_sm().text_color(colors.border).child(">"))
                            .child(div().text_sm().text_color(query_color).child(query_text)),
                    )
                    .child(div().pb_2().flex().flex_col().gap_0p5().children(rows)),
            )
    }
}

/// Return `color` with a replaced alpha channel.
fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

#[cfg(test)]
mod tests;
