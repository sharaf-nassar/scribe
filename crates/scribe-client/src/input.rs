//! Winit key event translation for terminal input and layout commands.

use scribe_common::config::KeybindingsConfig;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// A parsed key match target: either a single character or a named key.
#[derive(Debug, Clone)]
pub enum KeyMatch {
    /// A single character key (e.g. `'w'`, `'\\'`, `'-'`).
    Character(char),
    /// A named key (e.g. `Tab`, `Enter`).
    Named(NamedKey),
}

/// A parsed keybinding: modifier flags + key target.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent modifier flags (ctrl, shift, alt) are not a state machine"
)]
pub struct Keybinding {
    /// Whether the Ctrl modifier is required.
    pub ctrl: bool,
    /// Whether the Shift modifier is required.
    pub shift: bool,
    /// Whether the Alt modifier is required.
    pub alt: bool,
    /// The key that must be pressed.
    pub key: KeyMatch,
}

/// All parsed keybindings for the client.
#[derive(Debug, Clone)]
pub struct Bindings {
    // Panes
    pub split_vertical: Keybinding,
    pub split_horizontal: Keybinding,
    pub close_pane: Keybinding,
    pub cycle_pane: Keybinding,
    pub focus_left: Keybinding,
    pub focus_right: Keybinding,
    pub focus_up: Keybinding,
    pub focus_down: Keybinding,

    // Workspaces
    pub workspace_split_vertical: Keybinding,
    pub workspace_split_horizontal: Keybinding,
    pub cycle_workspace: Keybinding,

    // Tabs
    pub new_tab: Keybinding,
    pub close_tab: Keybinding,
    pub next_tab: Keybinding,
    pub prev_tab: Keybinding,
    pub select_tab_1: Keybinding,
    pub select_tab_2: Keybinding,
    pub select_tab_3: Keybinding,
    pub select_tab_4: Keybinding,
    pub select_tab_5: Keybinding,
    pub select_tab_6: Keybinding,
    pub select_tab_7: Keybinding,
    pub select_tab_8: Keybinding,
    pub select_tab_9: Keybinding,

    // Clipboard
    pub copy: Keybinding,
    pub paste: Keybinding,

    // Navigation
    pub scroll_up: Keybinding,
    pub scroll_down: Keybinding,
    pub scroll_top: Keybinding,
    pub scroll_bottom: Keybinding,
    pub find: Keybinding,

    // View
    pub zoom_in: Keybinding,
    pub zoom_out: Keybinding,
    pub zoom_reset: Keybinding,

    // Window
    pub new_window: Keybinding,

    // General
    pub settings: Keybinding,
}

impl Keybinding {
    /// Parse a keybinding string like `"ctrl+shift+w"` into a `Keybinding`.
    ///
    /// Returns `None` if the string is malformed or the key part is unrecognised.
    pub fn parse(s: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key_part: Option<String> = None;

        for part in s.split('+') {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                _ => key_part = Some(lower),
            }
        }

        let key = match key_part?.as_str() {
            "tab" => KeyMatch::Named(NamedKey::Tab),
            "enter" | "return" => KeyMatch::Named(NamedKey::Enter),
            "space" => KeyMatch::Named(NamedKey::Space),
            "backspace" => KeyMatch::Named(NamedKey::Backspace),
            "escape" | "esc" => KeyMatch::Named(NamedKey::Escape),
            "delete" => KeyMatch::Named(NamedKey::Delete),
            "left" => KeyMatch::Named(NamedKey::ArrowLeft),
            "right" => KeyMatch::Named(NamedKey::ArrowRight),
            "up" => KeyMatch::Named(NamedKey::ArrowUp),
            "down" => KeyMatch::Named(NamedKey::ArrowDown),
            "pageup" => KeyMatch::Named(NamedKey::PageUp),
            "pagedown" => KeyMatch::Named(NamedKey::PageDown),
            "home" => KeyMatch::Named(NamedKey::Home),
            "end" => KeyMatch::Named(NamedKey::End),
            ch if ch.len() == 1 => KeyMatch::Character(ch.chars().next()?),
            _ => return None,
        };

        Some(Self { ctrl, shift, alt, key })
    }

    /// Returns `true` if `event` with `modifiers` matches this keybinding.
    pub fn matches(&self, event: &KeyEvent, modifiers: ModifiersState) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        if self.ctrl != modifiers.control_key()
            || self.shift != modifiers.shift_key()
            || self.alt != modifiers.alt_key()
        {
            return false;
        }
        match &self.key {
            KeyMatch::Character(c) => {
                if let Key::Character(key_str) = &event.logical_key {
                    key_str.chars().next().is_some_and(|k| k.eq_ignore_ascii_case(c))
                } else {
                    false
                }
            }
            KeyMatch::Named(named) => {
                matches!(&event.logical_key, Key::Named(n) if n == named)
            }
        }
    }
}

impl Bindings {
    /// Parse all keybindings from config, falling back to defaults for invalid entries.
    pub fn parse(config: &KeybindingsConfig) -> Self {
        Self {
            // Panes
            split_vertical: parse_or_default(&config.split_vertical, "ctrl+shift+\\"),
            split_horizontal: parse_or_default(&config.split_horizontal, "ctrl+shift+-"),
            close_pane: parse_or_default(&config.close_pane, "ctrl+shift+w"),
            cycle_pane: parse_or_default(&config.cycle_pane, "ctrl+tab"),
            focus_left: parse_or_default(&config.focus_left, "alt+left"),
            focus_right: parse_or_default(&config.focus_right, "alt+right"),
            focus_up: parse_or_default(&config.focus_up, "alt+up"),
            focus_down: parse_or_default(&config.focus_down, "alt+down"),

            // Workspaces
            workspace_split_vertical: parse_or_default(
                &config.workspace_split_vertical,
                "ctrl+alt+\\",
            ),
            workspace_split_horizontal: parse_or_default(
                &config.workspace_split_horizontal,
                "ctrl+alt+-",
            ),
            cycle_workspace: parse_or_default(&config.cycle_workspace, "ctrl+alt+tab"),

            // Tabs
            new_tab: parse_or_default(&config.new_tab, "ctrl+shift+t"),
            close_tab: parse_or_default(&config.close_tab, "ctrl+shift+q"),
            next_tab: parse_or_default(&config.next_tab, "ctrl+pagedown"),
            prev_tab: parse_or_default(&config.prev_tab, "ctrl+pageup"),
            select_tab_1: parse_or_default(&config.select_tab_1, "ctrl+1"),
            select_tab_2: parse_or_default(&config.select_tab_2, "ctrl+2"),
            select_tab_3: parse_or_default(&config.select_tab_3, "ctrl+3"),
            select_tab_4: parse_or_default(&config.select_tab_4, "ctrl+4"),
            select_tab_5: parse_or_default(&config.select_tab_5, "ctrl+5"),
            select_tab_6: parse_or_default(&config.select_tab_6, "ctrl+6"),
            select_tab_7: parse_or_default(&config.select_tab_7, "ctrl+7"),
            select_tab_8: parse_or_default(&config.select_tab_8, "ctrl+8"),
            select_tab_9: parse_or_default(&config.select_tab_9, "ctrl+9"),

            // Clipboard
            copy: parse_or_default(&config.copy, "ctrl+shift+c"),
            paste: parse_or_default(&config.paste, "ctrl+shift+v"),

            // Navigation
            scroll_up: parse_or_default(&config.scroll_up, "shift+pageup"),
            scroll_down: parse_or_default(&config.scroll_down, "shift+pagedown"),
            scroll_top: parse_or_default(&config.scroll_top, "shift+home"),
            scroll_bottom: parse_or_default(&config.scroll_bottom, "shift+end"),
            find: parse_or_default(&config.find, "ctrl+shift+f"),

            // View
            zoom_in: parse_or_default(&config.zoom_in, "ctrl+="),
            zoom_out: parse_or_default(&config.zoom_out, "ctrl+-"),
            zoom_reset: parse_or_default(&config.zoom_reset, "ctrl+0"),

            // Window
            new_window: parse_or_default(&config.new_window, "ctrl+shift+n"),

            // General
            settings: parse_or_default(&config.settings, "ctrl+,"),
        }
    }
}

/// Parse a keybinding string, falling back to `default` with a warning if invalid.
fn parse_or_default(value: &str, default: &str) -> Keybinding {
    Keybinding::parse(value).unwrap_or_else(|| {
        tracing::warn!(binding = value, default, "invalid keybinding, using default");
        #[allow(
            clippy::expect_used,
            reason = "hardcoded default keybinding strings are guaranteed valid"
        )]
        Keybinding::parse(default).expect("default keybinding must parse")
    })
}

/// Layout commands intercepted before normal key translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
    // Panes
    /// Split the focused pane vertically (side-by-side).
    SplitVertical,
    /// Split the focused pane horizontally (top/bottom).
    SplitHorizontal,
    /// Close the focused pane.
    ClosePane,
    /// Cycle focus to the next pane.
    FocusNext,
    /// Move focus to the pane on the left.
    FocusLeft,
    /// Move focus to the pane on the right.
    FocusRight,
    /// Move focus to the pane above.
    FocusUp,
    /// Move focus to the pane below.
    FocusDown,

    // Workspaces
    /// Split the window to create a workspace side-by-side.
    WorkspaceSplitVertical,
    /// Split the window to create a workspace top/bottom.
    WorkspaceSplitHorizontal,
    /// Cycle focus to the next workspace.
    CycleWorkspaceFocus,

    // Tabs
    /// Create a new tab in the focused workspace.
    NewTab,
    /// Close the active tab in the focused workspace.
    CloseTab,
    /// Switch to the next tab.
    NextTab,
    /// Switch to the previous tab.
    PrevTab,
    /// Jump to a specific tab (0-indexed).
    SelectTab(usize),

    // Window
    /// Open a new window.
    NewWindow,

    // Clipboard
    /// Copy the current selection to the clipboard.
    CopySelection,
    /// Paste from the clipboard into the focused session.
    PasteClipboard,

    // Navigation
    /// Scroll up by one page in the focused pane.
    ScrollUp,
    /// Scroll down by one page in the focused pane.
    ScrollDown,
    /// Scroll to the top of the scrollback buffer.
    ScrollTop,
    /// Scroll to the bottom (live view).
    ScrollBottom,

    // View
    /// Increase the font size.
    ZoomIn,
    /// Decrease the font size.
    ZoomOut,
    /// Reset the font size to the configured default.
    ZoomReset,
}

/// Result of translating a winit key event.
#[derive(Debug)]
pub enum KeyAction {
    /// Terminal byte sequence to send to the PTY.
    Terminal(Vec<u8>),
    /// Layout command (split, close, focus, tabs, clipboard, etc.).
    Layout(LayoutAction),
    /// Open the settings window.
    OpenSettings,
    /// Open the find-in-scrollback overlay.
    OpenFind,
}

/// Translate a winit key event into either terminal bytes or a layout command.
///
/// Layout shortcuts are intercepted first using the provided `bindings`.
/// Returns `None` if the key should be ignored.
pub fn translate_key_action(
    event: &KeyEvent,
    modifiers: ModifiersState,
    bindings: &Bindings,
) -> Option<KeyAction> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Check for layout shortcuts first.
    if let Some(action) = translate_layout_shortcut(event, modifiers, bindings) {
        return Some(KeyAction::Layout(action));
    }

    if bindings.settings.matches(event, modifiers) {
        return Some(KeyAction::OpenSettings);
    }

    if bindings.find.matches(event, modifiers) {
        return Some(KeyAction::OpenFind);
    }

    // Fall through to normal terminal key translation.
    translate_key(event, modifiers).map(KeyAction::Terminal)
}

/// Translate a winit key event into terminal byte sequences.
///
/// Returns `None` if the key should be ignored (key-up events,
/// unrecognised keys, or modifier-only keys).
pub fn translate_key(event: &KeyEvent, modifiers: ModifiersState) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Ctrl+key takes priority over normal character output.
    if modifiers.control_key() {
        return translate_ctrl_key(event);
    }

    match &event.logical_key {
        Key::Character(c) => Some(c.as_bytes().to_vec()),
        Key::Named(named) => translate_named_key(*named),
        _ => None,
    }
}

/// Check for layout shortcuts using the provided bindings.
#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "flat sequential binding checks including tab selection are inherently simple despite the count"
)]
fn translate_layout_shortcut(
    event: &KeyEvent,
    modifiers: ModifiersState,
    bindings: &Bindings,
) -> Option<LayoutAction> {
    // Panes
    if bindings.split_vertical.matches(event, modifiers) {
        return Some(LayoutAction::SplitVertical);
    }
    if bindings.split_horizontal.matches(event, modifiers) {
        return Some(LayoutAction::SplitHorizontal);
    }
    if bindings.close_pane.matches(event, modifiers) {
        return Some(LayoutAction::ClosePane);
    }
    if bindings.cycle_pane.matches(event, modifiers) {
        return Some(LayoutAction::FocusNext);
    }
    if bindings.focus_left.matches(event, modifiers) {
        return Some(LayoutAction::FocusLeft);
    }
    if bindings.focus_right.matches(event, modifiers) {
        return Some(LayoutAction::FocusRight);
    }
    if bindings.focus_up.matches(event, modifiers) {
        return Some(LayoutAction::FocusUp);
    }
    if bindings.focus_down.matches(event, modifiers) {
        return Some(LayoutAction::FocusDown);
    }

    // Workspaces
    if bindings.workspace_split_vertical.matches(event, modifiers) {
        return Some(LayoutAction::WorkspaceSplitVertical);
    }
    if bindings.workspace_split_horizontal.matches(event, modifiers) {
        return Some(LayoutAction::WorkspaceSplitHorizontal);
    }
    if bindings.cycle_workspace.matches(event, modifiers) {
        return Some(LayoutAction::CycleWorkspaceFocus);
    }

    // Window
    if bindings.new_window.matches(event, modifiers) {
        return Some(LayoutAction::NewWindow);
    }

    // Tabs
    if bindings.new_tab.matches(event, modifiers) {
        return Some(LayoutAction::NewTab);
    }
    if bindings.close_tab.matches(event, modifiers) {
        return Some(LayoutAction::CloseTab);
    }
    if bindings.next_tab.matches(event, modifiers) {
        return Some(LayoutAction::NextTab);
    }
    if bindings.prev_tab.matches(event, modifiers) {
        return Some(LayoutAction::PrevTab);
    }
    if bindings.select_tab_1.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(0));
    }
    if bindings.select_tab_2.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(1));
    }
    if bindings.select_tab_3.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(2));
    }
    if bindings.select_tab_4.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(3));
    }
    if bindings.select_tab_5.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(4));
    }
    if bindings.select_tab_6.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(5));
    }
    if bindings.select_tab_7.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(6));
    }
    if bindings.select_tab_8.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(7));
    }
    if bindings.select_tab_9.matches(event, modifiers) {
        return Some(LayoutAction::SelectTab(8));
    }

    // Clipboard
    if bindings.copy.matches(event, modifiers) {
        return Some(LayoutAction::CopySelection);
    }
    if bindings.paste.matches(event, modifiers) {
        return Some(LayoutAction::PasteClipboard);
    }

    // Navigation
    if bindings.scroll_up.matches(event, modifiers) {
        return Some(LayoutAction::ScrollUp);
    }
    if bindings.scroll_down.matches(event, modifiers) {
        return Some(LayoutAction::ScrollDown);
    }
    if bindings.scroll_top.matches(event, modifiers) {
        return Some(LayoutAction::ScrollTop);
    }
    if bindings.scroll_bottom.matches(event, modifiers) {
        return Some(LayoutAction::ScrollBottom);
    }

    // View
    if bindings.zoom_in.matches(event, modifiers) {
        return Some(LayoutAction::ZoomIn);
    }
    if bindings.zoom_out.matches(event, modifiers) {
        return Some(LayoutAction::ZoomOut);
    }
    if bindings.zoom_reset.matches(event, modifiers) {
        return Some(LayoutAction::ZoomReset);
    }

    None
}

/// Translate a named key to its VT100 / ANSI byte sequence.
fn translate_named_key(named: NamedKey) -> Option<Vec<u8>> {
    let seq: &[u8] = match named {
        NamedKey::Space => b" ",
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::Insert => b"\x1b[2~",
        NamedKey::F1 => b"\x1bOP",
        NamedKey::F2 => b"\x1bOQ",
        NamedKey::F3 => b"\x1bOR",
        NamedKey::F4 => b"\x1bOS",
        NamedKey::F5 => b"\x1b[15~",
        NamedKey::F6 => b"\x1b[17~",
        NamedKey::F7 => b"\x1b[18~",
        NamedKey::F8 => b"\x1b[19~",
        NamedKey::F9 => b"\x1b[20~",
        NamedKey::F10 => b"\x1b[21~",
        NamedKey::F11 => b"\x1b[23~",
        NamedKey::F12 => b"\x1b[24~",
        NamedKey::F13 => b"\x1b[25~",
        NamedKey::F14 => b"\x1b[26~",
        NamedKey::F15 => b"\x1b[28~",
        NamedKey::F16 => b"\x1b[29~",
        NamedKey::F17 => b"\x1b[31~",
        NamedKey::F18 => b"\x1b[32~",
        NamedKey::F19 => b"\x1b[33~",
        NamedKey::F20 => b"\x1b[34~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Translate a Ctrl+key combination to a control byte (0x01-0x1a).
fn translate_ctrl_key(event: &KeyEvent) -> Option<Vec<u8>> {
    match &event.logical_key {
        Key::Character(c) => {
            let ch = c.chars().next()?;
            if ch.is_ascii_lowercase() {
                #[allow(
                    clippy::as_conversions,
                    reason = "ASCII lowercase char is guaranteed to fit in u8"
                )]
                Some(vec![ch as u8 - b'a' + 1])
            } else if ch.is_ascii_uppercase() {
                #[allow(
                    clippy::as_conversions,
                    reason = "ASCII uppercase char is guaranteed to fit in u8"
                )]
                Some(vec![ch as u8 - b'A' + 1])
            } else {
                None
            }
        }
        // Ctrl+Space sends NUL (used as tmux prefix, emacs set-mark, etc.).
        Key::Named(NamedKey::Space) => Some(vec![0]),
        _ => None,
    }
}
