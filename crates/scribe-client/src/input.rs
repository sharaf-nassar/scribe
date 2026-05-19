//! Winit key event translation for terminal input and layout commands.

use scribe_common::config::{KeyComboList, KeybindingsConfig};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

/// A set of parsed keybindings for a single action (one or more combos).
pub type BindingSet = Vec<Keybinding>;

/// Returns `true` if any binding in `set` matches the event and modifiers.
pub fn any_matches(set: &BindingSet, event: &KeyEvent, modifiers: ModifiersState) -> bool {
    set.iter().any(|b| b.matches(event, modifiers))
}

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
pub struct Keybinding {
    /// The exact modifier bitset required for this keybinding.
    pub modifiers: ModifiersState,
    /// The key that must be pressed.
    pub key: KeyMatch,
}

/// All parsed keybindings for the client.
#[derive(Debug, Clone)]
pub struct Bindings {
    // Panes
    pub split_vertical: BindingSet,
    pub split_horizontal: BindingSet,
    pub close_pane: BindingSet,
    pub cycle_pane: BindingSet,
    pub focus_left: BindingSet,
    pub focus_right: BindingSet,
    pub focus_up: BindingSet,
    pub focus_down: BindingSet,

    // Workspaces
    pub workspace_split_vertical: BindingSet,
    pub workspace_split_horizontal: BindingSet,
    pub workspace_focus_left: BindingSet,
    pub workspace_focus_right: BindingSet,
    pub workspace_focus_up: BindingSet,
    pub workspace_focus_down: BindingSet,

    // Tabs
    pub new_tab: BindingSet,
    pub new_claude_tab: BindingSet,
    pub new_claude_resume_tab: BindingSet,
    pub new_codex_tab: BindingSet,
    pub new_codex_resume_tab: BindingSet,
    pub close_tab: BindingSet,
    pub next_tab: BindingSet,
    pub prev_tab: BindingSet,
    pub select_tab_1: BindingSet,
    pub select_tab_2: BindingSet,
    pub select_tab_3: BindingSet,
    pub select_tab_4: BindingSet,
    pub select_tab_5: BindingSet,
    pub select_tab_6: BindingSet,
    pub select_tab_7: BindingSet,
    pub select_tab_8: BindingSet,
    pub select_tab_9: BindingSet,

    // Clipboard
    pub copy: BindingSet,
    pub paste: BindingSet,

    // Navigation
    pub scroll_up: BindingSet,
    pub scroll_down: BindingSet,
    pub scroll_top: BindingSet,
    pub scroll_bottom: BindingSet,
    pub find: BindingSet,
    pub prompt_jump_up: BindingSet,
    pub prompt_jump_down: BindingSet,
    pub jump_to_failure: BindingSet,

    // View
    pub zoom_in: BindingSet,
    pub zoom_out: BindingSet,
    pub zoom_reset: BindingSet,

    // Window
    pub new_window: BindingSet,

    // General
    pub command_palette: BindingSet,
    pub settings: BindingSet,

    // Terminal shortcuts (send escape sequences to PTY)
    pub word_left: BindingSet,
    pub word_right: BindingSet,
    pub delete_word_backward: BindingSet,
    pub delete_word_backward_ctrl: BindingSet,
    pub delete_word_forward: BindingSet,
    pub line_start: BindingSet,
    pub line_end: BindingSet,
}

impl Keybinding {
    /// Parse a keybinding string like `"ctrl+shift+w"` into a `Keybinding`.
    ///
    /// Returns `None` if the string is malformed or the key part is unrecognised.
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = ModifiersState::empty();
        let mut key_part: Option<String> = None;

        for part in s.split('+') {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" => modifiers.insert(ModifiersState::CONTROL),
                "shift" => modifiers.insert(ModifiersState::SHIFT),
                "alt" => modifiers.insert(ModifiersState::ALT),
                "cmd" | "super" => modifiers.insert(ModifiersState::SUPER),
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

        Some(Self { modifiers, key })
    }

    /// Returns `true` if `event` with `modifiers` matches this keybinding.
    pub fn matches(&self, event: &KeyEvent, modifiers: ModifiersState) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        if self.modifiers != modifiers {
            return false;
        }
        match &self.key {
            KeyMatch::Character(c) => {
                if let Key::Character(key_str) = &event.key_without_modifiers() {
                    key_str.chars().next().is_some_and(|k| k.eq_ignore_ascii_case(c))
                } else {
                    false
                }
            }
            KeyMatch::Named(named) => {
                matches!(event.key_without_modifiers(), Key::Named(n) if n == *named)
            }
        }
    }
}

impl Bindings {
    /// Parse all keybindings from config.
    ///
    /// Defaults are defined in [`KeybindingsConfig::default()`] (the single
    /// source of truth).  Serde fills them in for any missing config fields,
    /// so every list is non-empty by the time it reaches here.  Invalid
    /// entries are skipped with a warning.
    pub fn parse(config: &KeybindingsConfig) -> Self {
        Self {
            // Panes
            split_vertical: parse_set(&config.split_vertical),
            split_horizontal: parse_set(&config.split_horizontal),
            close_pane: parse_set(&config.close_pane),
            cycle_pane: parse_set(&config.cycle_pane),
            focus_left: parse_set(&config.focus_left),
            focus_right: parse_set(&config.focus_right),
            focus_up: parse_set(&config.focus_up),
            focus_down: parse_set(&config.focus_down),

            // Workspaces
            workspace_split_vertical: parse_set(&config.workspace_split_vertical),
            workspace_split_horizontal: parse_set(&config.workspace_split_horizontal),
            workspace_focus_left: parse_set(&config.workspace_focus_left),
            workspace_focus_right: parse_set(&config.workspace_focus_right),
            workspace_focus_up: parse_set(&config.workspace_focus_up),
            workspace_focus_down: parse_set(&config.workspace_focus_down),

            // Tabs
            new_tab: parse_set(&config.new_tab),
            new_claude_tab: parse_set(&config.new_claude_tab),
            new_claude_resume_tab: parse_set(&config.new_claude_resume_tab),
            new_codex_tab: parse_set(&config.new_codex_tab),
            new_codex_resume_tab: parse_set(&config.new_codex_resume_tab),
            close_tab: parse_set(&config.close_tab),
            next_tab: parse_set(&config.next_tab),
            prev_tab: parse_set(&config.prev_tab),
            select_tab_1: parse_set(&config.select_tab_1),
            select_tab_2: parse_set(&config.select_tab_2),
            select_tab_3: parse_set(&config.select_tab_3),
            select_tab_4: parse_set(&config.select_tab_4),
            select_tab_5: parse_set(&config.select_tab_5),
            select_tab_6: parse_set(&config.select_tab_6),
            select_tab_7: parse_set(&config.select_tab_7),
            select_tab_8: parse_set(&config.select_tab_8),
            select_tab_9: parse_set(&config.select_tab_9),

            // Clipboard
            copy: parse_set(&config.copy),
            paste: parse_set(&config.paste),

            // Navigation
            scroll_up: parse_set(&config.scroll_up),
            scroll_down: parse_set(&config.scroll_down),
            scroll_top: parse_set(&config.scroll_top),
            scroll_bottom: parse_set(&config.scroll_bottom),
            find: parse_set(&config.find),
            prompt_jump_up: parse_set(&config.prompt_jump_up),
            prompt_jump_down: parse_set(&config.prompt_jump_down),
            jump_to_failure: parse_set(&config.jump_to_failure),

            // View
            zoom_in: parse_set(&config.zoom_in),
            zoom_out: parse_set(&config.zoom_out),
            zoom_reset: parse_set(&config.zoom_reset),

            // Window
            new_window: parse_set(&config.new_window),

            // General
            command_palette: parse_set(&config.command_palette),
            settings: parse_set(&config.settings),

            // Terminal shortcuts
            word_left: parse_set(&config.word_left),
            word_right: parse_set(&config.word_right),
            delete_word_backward: parse_set(&config.delete_word_backward),
            delete_word_backward_ctrl: parse_set(&config.delete_word_backward_ctrl),
            delete_word_forward: parse_set(&config.delete_word_forward),
            line_start: parse_set(&config.line_start),
            line_end: parse_set(&config.line_end),
        }
    }
}

/// Parse a combo list into a [`BindingSet`], skipping invalid entries.
///
/// Returns an empty set if the list is empty or all entries are invalid.
/// Defaults are provided by [`KeybindingsConfig::default()`] via serde,
/// so the list is always populated for well-formed configs.
fn parse_set(list: &KeyComboList) -> BindingSet {
    list.as_slice()
        .iter()
        .filter_map(|s| {
            let kb = Keybinding::parse(s);
            if kb.is_none() {
                tracing::warn!(binding = s.as_str(), "invalid keybinding string, skipping");
            }
            kb
        })
        .collect()
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
    /// Move focus to the workspace on the left.
    WorkspaceFocusLeft,
    /// Move focus to the workspace on the right.
    WorkspaceFocusRight,
    /// Move focus to the workspace above.
    WorkspaceFocusUp,
    /// Move focus to the workspace below.
    WorkspaceFocusDown,

    // Tabs
    /// Create a new tab in the focused workspace.
    NewTab,
    /// Open a new tab running Claude Code in the focused workspace.
    NewClaudeTab,
    /// Open a new tab resuming Claude Code in the focused workspace.
    NewClaudeResumeTab,
    /// Open a new tab running Codex in the focused workspace.
    NewCodexTab,
    /// Open a new tab resuming Codex in the focused workspace.
    NewCodexResumeTab,
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
    /// Jump to the previous prompt mark.
    PromptJumpUp,
    /// Jump to the next prompt mark.
    PromptJumpDown,
    /// Jump to the most recent failed command.
    JumpToFailure,

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
    /// Open the command palette overlay.
    OpenCommandPalette,
    /// Open the find-in-scrollback overlay.
    OpenFind,
}

/// The set of Kitty keyboard-protocol progressive-enhancement flags currently
/// negotiated by the focused terminal application.
///
/// This models the five independent flags of the published Kitty keyboard
/// protocol (`disambiguate escape codes`, `report event types`, `report
/// alternate keys`, `report all keys as escape codes`, `report associated
/// text`). Apps negotiate arbitrary subsets, so each flag must be honored
/// independently; an all-false value means pure legacy encoding.
///
/// The five flags are stored as a small bitfield rather than five `bool`
/// fields purely to satisfy the workspace `clippy::struct_excessive_bools`
/// gate (`max-struct-bools = 2`); the public surface is the five flag
/// accessors below plus [`KittyFlags::is_any`]/[`KittyFlags::legacy`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KittyFlags {
    bits: u8,
}

impl KittyFlags {
    /// `CSI = 1 u` — disambiguate escape codes.
    const DISAMBIGUATE: u8 = 1 << 0;
    /// `CSI = 2 u` — report event types (press/repeat/release).
    const REPORT_EVENT_TYPES: u8 = 1 << 1;
    /// `CSI = 4 u` — report alternate (shifted/base-layout) keys.
    const REPORT_ALTERNATE_KEYS: u8 = 1 << 2;
    /// `CSI = 8 u` — report all keys as escape codes.
    const REPORT_ALL_KEYS: u8 = 1 << 3;
    /// `CSI = 16 u` — report associated text.
    const REPORT_ASSOCIATED_TEXT: u8 = 1 << 4;

    /// All flags off — pure legacy encoding (the [`KittyFlags::legacy`] state).
    ///
    /// The negotiated flags are layered on with the `with_*` builders, each
    /// taking a single `bool` so the focused-pane derivation can map the five
    /// independent `TermMode` bits directly without a wide multi-bool
    /// constructor.
    #[must_use]
    pub const fn legacy_set() -> Self {
        Self { bits: 0 }
    }

    #[must_use]
    const fn with_bit(self, bit: u8, on: bool) -> Self {
        if on { Self { bits: self.bits | bit } } else { self }
    }

    /// Set `disambiguate escape codes` (`CSI = 1 u`).
    #[must_use]
    pub const fn with_disambiguate(self, on: bool) -> Self {
        self.with_bit(Self::DISAMBIGUATE, on)
    }

    /// Set `report event types` (`CSI = 2 u`).
    #[must_use]
    pub const fn with_report_event_types(self, on: bool) -> Self {
        self.with_bit(Self::REPORT_EVENT_TYPES, on)
    }

    /// Set `report alternate keys` (`CSI = 4 u`).
    #[must_use]
    pub const fn with_report_alternate_keys(self, on: bool) -> Self {
        self.with_bit(Self::REPORT_ALTERNATE_KEYS, on)
    }

    /// Set `report all keys as escape codes` (`CSI = 8 u`).
    #[must_use]
    pub const fn with_report_all_keys(self, on: bool) -> Self {
        self.with_bit(Self::REPORT_ALL_KEYS, on)
    }

    /// Set `report associated text` (`CSI = 16 u`).
    #[must_use]
    pub const fn with_report_associated_text(self, on: bool) -> Self {
        self.with_bit(Self::REPORT_ASSOCIATED_TEXT, on)
    }

    /// `disambiguate escape codes` negotiated.
    #[must_use]
    pub const fn disambiguate(self) -> bool {
        self.bits & Self::DISAMBIGUATE != 0
    }

    /// `report event types` negotiated.
    #[must_use]
    pub const fn report_event_types(self) -> bool {
        self.bits & Self::REPORT_EVENT_TYPES != 0
    }

    /// `report alternate keys` negotiated.
    #[must_use]
    pub const fn report_alternate_keys(self) -> bool {
        self.bits & Self::REPORT_ALTERNATE_KEYS != 0
    }

    /// `report all keys as escape codes` negotiated.
    #[must_use]
    pub const fn report_all_keys(self) -> bool {
        self.bits & Self::REPORT_ALL_KEYS != 0
    }

    /// `report associated text` negotiated.
    #[must_use]
    pub const fn report_associated_text(self) -> bool {
        self.bits & Self::REPORT_ASSOCIATED_TEXT != 0
    }

    /// `true` when at least one enhancement flag is negotiated.
    #[must_use]
    pub const fn is_any(self) -> bool {
        self.bits != 0
    }

    /// `true` when no enhancement flag is negotiated (legacy encoding).
    ///
    /// When this holds, every translation path is byte-identical to the
    /// pre-feature legacy behavior (SC-003).
    #[must_use]
    pub const fn legacy(self) -> bool {
        self.bits == 0
    }
}

/// Translate a winit key event into either terminal bytes or a layout command.
///
/// Priority: layout shortcuts → settings/find → terminal shortcuts → generic key translation.
/// Returns `None` if the key should be ignored.
pub fn translate_key_action(
    event: &KeyEvent,
    modifiers: ModifiersState,
    bindings: &Bindings,
    flags: KittyFlags,
) -> Option<KeyAction> {
    // Levels 1–3 (layout shortcuts, palette/settings/find, terminal
    // shortcuts) are press-only and unaffected by the keyboard protocol.
    if event.state == ElementState::Pressed {
        if let Some(action) = translate_layout_shortcut(event, modifiers, bindings) {
            return Some(KeyAction::Layout(action));
        }

        if any_matches(&bindings.command_palette, event, modifiers) {
            return Some(KeyAction::OpenCommandPalette);
        }

        if any_matches(&bindings.settings, event, modifiers) {
            return Some(KeyAction::OpenSettings);
        }

        if any_matches(&bindings.find, event, modifiers) {
            return Some(KeyAction::OpenFind);
        }

        // Check configurable terminal shortcuts (specific escape sequences).
        if let Some(bytes) = translate_terminal_shortcut(event, modifiers, bindings) {
            return Some(KeyAction::Terminal(bytes));
        }
    }

    // Level 4: generic terminal key translation with modifier encoding.
    translate_key(event, modifiers, flags).map(KeyAction::Terminal)
}

/// Translate a winit key event into terminal byte sequences.
///
/// Handles modifier encoding for all key combinations:
/// - Ctrl+character → control byte (0x01–0x1a)
/// - Ctrl+Alt+character → ESC + control byte
/// - Alt+character → ESC + character
/// - Modified Enter in kitty mode → CSI-u sequence
/// - Modifier+named-key → xterm modifier-encoded escape sequence
///
/// Returns `None` if the key should be ignored (key-up events,
/// unrecognised keys, or modifier-only keys).
pub fn translate_key(
    event: &KeyEvent,
    modifiers: ModifiersState,
    flags: KittyFlags,
) -> Option<Vec<u8>> {
    // Legacy fast path: when no enhancement flag is negotiated, behavior is
    // byte-identical to the pre-feature implementation (SC-003 / T011). The
    // press-only gate and the exact legacy branches below are untouched.
    if flags.legacy() {
        if event.state != ElementState::Pressed {
            return None;
        }
        return match &event.logical_key {
            Key::Character(c) => translate_character_with_modifiers(c, modifiers),
            Key::Named(named) => translate_named_legacy(*named, modifiers),
            _ => None,
        };
    }

    translate_key_kitty(event, modifiers, flags)
}

/// Check for layout shortcuts using the provided bindings.
fn translate_layout_shortcut(
    event: &KeyEvent,
    modifiers: ModifiersState,
    bindings: &Bindings,
) -> Option<LayoutAction> {
    let pane_actions = pane_layout_actions(bindings);
    let workspace_actions = workspace_layout_actions(bindings);
    let tab_actions = tab_layout_actions(bindings);
    let view_actions = view_layout_actions(bindings);

    [
        pane_actions.as_slice(),
        workspace_actions.as_slice(),
        tab_actions.as_slice(),
        view_actions.as_slice(),
    ]
    .iter()
    .find_map(|actions| match_binding_actions(event, modifiers, actions))
}

/// Check configurable terminal shortcut bindings.
///
/// Each binding maps a key combination to a fixed escape sequence sent to the PTY.
fn translate_terminal_shortcut(
    event: &KeyEvent,
    modifiers: ModifiersState,
    bindings: &Bindings,
) -> Option<Vec<u8>> {
    const WORD_LEFT: &[u8] = b"\x1b[1;5D";
    const WORD_RIGHT: &[u8] = b"\x1b[1;5C";
    const DELETE_WORD_BACKWARD: &[u8] = &[0x1b, 0x7f];
    const DELETE_WORD_BACKWARD_CTRL: &[u8] = &[0x08];
    const DELETE_WORD_FORWARD: &[u8] = b"\x1b[3;5~";
    const LINE_START: &[u8] = b"\x1b[1;5H";
    const LINE_END: &[u8] = b"\x1b[1;5F";

    let shortcuts: [BindingAction<'_, &[u8]>; 7] = [
        BindingAction { bindings: &bindings.word_left, action: WORD_LEFT },
        BindingAction { bindings: &bindings.word_right, action: WORD_RIGHT },
        BindingAction { bindings: &bindings.delete_word_backward, action: DELETE_WORD_BACKWARD },
        BindingAction {
            bindings: &bindings.delete_word_backward_ctrl,
            action: DELETE_WORD_BACKWARD_CTRL,
        },
        BindingAction { bindings: &bindings.delete_word_forward, action: DELETE_WORD_FORWARD },
        BindingAction { bindings: &bindings.line_start, action: LINE_START },
        BindingAction { bindings: &bindings.line_end, action: LINE_END },
    ];

    shortcuts.iter().find_map(|entry| {
        any_matches(entry.bindings, event, modifiers).then(|| entry.action.to_vec())
    })
}

// ---------------------------------------------------------------------------
// Generic terminal key translation with modifier encoding
// ---------------------------------------------------------------------------

/// Translate a character key with modifier encoding.
///
/// - Ctrl+char → control byte (0x01–0x1a)
/// - Ctrl+Alt+char → ESC + control byte
/// - Alt+char → ESC + character bytes
/// - No relevant modifiers → raw character bytes
fn translate_character_with_modifiers(c: &str, modifiers: ModifiersState) -> Option<Vec<u8>> {
    // Drop Cmd/Super combos that didn't match any binding — on macOS these are
    // OS-level shortcuts and sending raw chars to the PTY would be wrong.
    if modifiers.super_key() {
        return None;
    }

    let ctrl = modifiers.control_key();
    let alt = modifiers.alt_key();

    if ctrl {
        let control_byte = char_to_control_byte(c)?;
        if alt {
            // Ctrl+Alt+char → ESC + control byte
            Some(vec![0x1b, control_byte])
        } else {
            // Ctrl+char → control byte
            Some(vec![control_byte])
        }
    } else if alt {
        // Alt+char → ESC + character bytes
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(c.as_bytes());
        Some(bytes)
    } else {
        // No relevant modifiers → raw character bytes
        Some(c.as_bytes().to_vec())
    }
}

/// Convert a character to its Ctrl control byte.
///
/// Maps a–z / A–Z to 0x01–0x1a. Space is handled separately via `NamedKey::Space`.
fn char_to_control_byte(c: &str) -> Option<u8> {
    let ch = c.chars().next()?;
    let ch = u8::try_from(u32::from(ch)).ok()?;
    if ch.is_ascii_lowercase() {
        Some(ch - b'a' + 1)
    } else if ch.is_ascii_uppercase() {
        Some(ch - b'A' + 1)
    } else {
        None
    }
}

/// Translate a named key with xterm modifier encoding.
///
/// When modifiers are held, encodes them using the standard xterm parameter:
/// `param = 1 + shift(1) + alt(2) + ctrl(4)`.
///
/// Special cases (Backspace, Space, Enter, Tab, Escape) are handled separately
/// since they don't all follow the standard CSI modifier encoding.
/// Legacy named-key translation (byte-identical to the pre-feature path).
///
/// Used only when no Kitty enhancement flag is negotiated. The Kitty path
/// lives in [`translate_named_kitty`].
fn translate_named_legacy(named: NamedKey, modifiers: ModifiersState) -> Option<Vec<u8>> {
    // Drop Cmd/Super combos that didn't match any binding — on macOS these are
    // OS-level shortcuts and sending wrong PTY sequences would be incorrect.
    if modifiers.super_key() {
        return None;
    }

    if let Some(bytes) = translate_named_special(named, modifiers) {
        return Some(bytes);
    }

    // Compute xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let modifier_param = xterm_modifier_param(modifiers);

    translate_named_csi_letter(named, modifier_param)
        .or_else(|| translate_named_csi_tilde(named, modifier_param))
        .or_else(|| translate_named_function_key(named, modifier_param))
}

fn translate_named_special(named: NamedKey, modifiers: ModifiersState) -> Option<Vec<u8>> {
    match named {
        NamedKey::Backspace => {
            if modifiers.control_key() && modifiers.alt_key() {
                Some(vec![0x1b, 0x08])
            } else if modifiers.alt_key() {
                Some(vec![0x1b, 0x7f])
            } else if modifiers.control_key() {
                Some(vec![0x08])
            } else {
                Some(vec![0x7f])
            }
        }
        NamedKey::Space => {
            if modifiers.control_key() {
                Some(vec![0])
            } else if modifiers.alt_key() {
                Some(vec![0x1b, b' '])
            } else {
                Some(b" ".to_vec())
            }
        }
        NamedKey::Enter => {
            if modifiers.alt_key() {
                Some(vec![0x1b, b'\r'])
            } else {
                Some(b"\r".to_vec())
            }
        }
        NamedKey::Tab => {
            if modifiers.shift_key() {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(b"\t".to_vec())
            }
        }
        NamedKey::Escape => Some(b"\x1b".to_vec()),
        _ => None,
    }
}

fn translate_named_csi_letter(named: NamedKey, modifier_param: Option<u8>) -> Option<Vec<u8>> {
    csi_letter_for_named(named).map(|letter| build_csi_letter_seq(letter, modifier_param))
}

fn translate_named_csi_tilde(named: NamedKey, modifier_param: Option<u8>) -> Option<Vec<u8>> {
    csi_tilde_code_for_named(named).map(|code| build_csi_tilde_seq(code, modifier_param))
}

fn translate_named_function_key(named: NamedKey, modifier_param: Option<u8>) -> Option<Vec<u8>> {
    ss3_letter_for_fkey(named)
        .map(|letter| {
            modifier_param.map_or_else(
                || vec![0x1b, b'O', letter],
                |param| build_csi_letter_seq(letter, Some(param)),
            )
        })
        .or_else(|| fkey_tilde_code(named).map(|code| build_csi_tilde_seq(code, modifier_param)))
}

struct BindingAction<'a, T> {
    bindings: &'a BindingSet,
    action: T,
}

fn pane_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 8] {
    [
        BindingAction { bindings: &bindings.split_vertical, action: LayoutAction::SplitVertical },
        BindingAction {
            bindings: &bindings.split_horizontal,
            action: LayoutAction::SplitHorizontal,
        },
        BindingAction { bindings: &bindings.close_pane, action: LayoutAction::ClosePane },
        BindingAction { bindings: &bindings.cycle_pane, action: LayoutAction::FocusNext },
        BindingAction { bindings: &bindings.focus_left, action: LayoutAction::FocusLeft },
        BindingAction { bindings: &bindings.focus_right, action: LayoutAction::FocusRight },
        BindingAction { bindings: &bindings.focus_up, action: LayoutAction::FocusUp },
        BindingAction { bindings: &bindings.focus_down, action: LayoutAction::FocusDown },
    ]
}

fn workspace_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 6] {
    [
        BindingAction {
            bindings: &bindings.workspace_split_vertical,
            action: LayoutAction::WorkspaceSplitVertical,
        },
        BindingAction {
            bindings: &bindings.workspace_split_horizontal,
            action: LayoutAction::WorkspaceSplitHorizontal,
        },
        BindingAction {
            bindings: &bindings.workspace_focus_left,
            action: LayoutAction::WorkspaceFocusLeft,
        },
        BindingAction {
            bindings: &bindings.workspace_focus_right,
            action: LayoutAction::WorkspaceFocusRight,
        },
        BindingAction {
            bindings: &bindings.workspace_focus_up,
            action: LayoutAction::WorkspaceFocusUp,
        },
        BindingAction {
            bindings: &bindings.workspace_focus_down,
            action: LayoutAction::WorkspaceFocusDown,
        },
    ]
}

fn tab_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 18] {
    [
        BindingAction { bindings: &bindings.new_window, action: LayoutAction::NewWindow },
        BindingAction { bindings: &bindings.new_claude_tab, action: LayoutAction::NewClaudeTab },
        BindingAction {
            bindings: &bindings.new_claude_resume_tab,
            action: LayoutAction::NewClaudeResumeTab,
        },
        BindingAction { bindings: &bindings.new_codex_tab, action: LayoutAction::NewCodexTab },
        BindingAction {
            bindings: &bindings.new_codex_resume_tab,
            action: LayoutAction::NewCodexResumeTab,
        },
        BindingAction { bindings: &bindings.new_tab, action: LayoutAction::NewTab },
        BindingAction { bindings: &bindings.close_tab, action: LayoutAction::CloseTab },
        BindingAction { bindings: &bindings.next_tab, action: LayoutAction::NextTab },
        BindingAction { bindings: &bindings.prev_tab, action: LayoutAction::PrevTab },
        BindingAction { bindings: &bindings.select_tab_1, action: LayoutAction::SelectTab(0) },
        BindingAction { bindings: &bindings.select_tab_2, action: LayoutAction::SelectTab(1) },
        BindingAction { bindings: &bindings.select_tab_3, action: LayoutAction::SelectTab(2) },
        BindingAction { bindings: &bindings.select_tab_4, action: LayoutAction::SelectTab(3) },
        BindingAction { bindings: &bindings.select_tab_5, action: LayoutAction::SelectTab(4) },
        BindingAction { bindings: &bindings.select_tab_6, action: LayoutAction::SelectTab(5) },
        BindingAction { bindings: &bindings.select_tab_7, action: LayoutAction::SelectTab(6) },
        BindingAction { bindings: &bindings.select_tab_8, action: LayoutAction::SelectTab(7) },
        BindingAction { bindings: &bindings.select_tab_9, action: LayoutAction::SelectTab(8) },
    ]
}

fn view_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 12] {
    [
        BindingAction { bindings: &bindings.copy, action: LayoutAction::CopySelection },
        BindingAction { bindings: &bindings.paste, action: LayoutAction::PasteClipboard },
        BindingAction { bindings: &bindings.scroll_up, action: LayoutAction::ScrollUp },
        BindingAction { bindings: &bindings.scroll_down, action: LayoutAction::ScrollDown },
        BindingAction { bindings: &bindings.scroll_top, action: LayoutAction::ScrollTop },
        BindingAction { bindings: &bindings.scroll_bottom, action: LayoutAction::ScrollBottom },
        BindingAction { bindings: &bindings.prompt_jump_up, action: LayoutAction::PromptJumpUp },
        BindingAction {
            bindings: &bindings.prompt_jump_down,
            action: LayoutAction::PromptJumpDown,
        },
        BindingAction { bindings: &bindings.jump_to_failure, action: LayoutAction::JumpToFailure },
        BindingAction { bindings: &bindings.zoom_in, action: LayoutAction::ZoomIn },
        BindingAction { bindings: &bindings.zoom_out, action: LayoutAction::ZoomOut },
        BindingAction { bindings: &bindings.zoom_reset, action: LayoutAction::ZoomReset },
    ]
}

fn match_binding_actions<T: Copy>(
    event: &KeyEvent,
    modifiers: ModifiersState,
    candidates: &[BindingAction<'_, T>],
) -> Option<T> {
    candidates
        .iter()
        .find_map(|entry| any_matches(entry.bindings, event, modifiers).then_some(entry.action))
}

/// Compute the xterm modifier parameter.
///
/// Returns `None` when no modifiers are held (parameter is omitted),
/// or `Some(param)` where `param = 1 + shift(1) + alt(2) + ctrl(4)`.
fn xterm_modifier_param(modifiers: ModifiersState) -> Option<u8> {
    let mut param: u8 = 1;
    if modifiers.shift_key() {
        param += 1;
    }
    if modifiers.alt_key() {
        param += 2;
    }
    if modifiers.control_key() {
        param += 4;
    }
    if param > 1 { Some(param) } else { None }
}

/// Map arrow/home/end keys to their CSI letter.
fn csi_letter_for_named(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::ArrowUp => Some(b'A'),
        NamedKey::ArrowDown => Some(b'B'),
        NamedKey::ArrowRight => Some(b'C'),
        NamedKey::ArrowLeft => Some(b'D'),
        NamedKey::Home => Some(b'H'),
        NamedKey::End => Some(b'F'),
        _ => None,
    }
}

/// Map keys to their CSI tilde code number.
fn csi_tilde_code_for_named(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::Insert => Some(2),
        NamedKey::Delete => Some(3),
        NamedKey::PageUp => Some(5),
        NamedKey::PageDown => Some(6),
        _ => None,
    }
}

/// Map F1–F4 to their SS3 letter (P, Q, R, S).
fn ss3_letter_for_fkey(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::F1 => Some(b'P'),
        NamedKey::F2 => Some(b'Q'),
        NamedKey::F3 => Some(b'R'),
        NamedKey::F4 => Some(b'S'),
        _ => None,
    }
}

/// Map F5–F20 to their CSI tilde code.
fn fkey_tilde_code(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::F5 => Some(15),
        NamedKey::F6 => Some(17),
        NamedKey::F7 => Some(18),
        NamedKey::F8 => Some(19),
        NamedKey::F9 => Some(20),
        NamedKey::F10 => Some(21),
        NamedKey::F11 => Some(23),
        NamedKey::F12 => Some(24),
        NamedKey::F13 => Some(25),
        NamedKey::F14 => Some(26),
        NamedKey::F15 => Some(28),
        NamedKey::F16 => Some(29),
        NamedKey::F17 => Some(31),
        NamedKey::F18 => Some(32),
        NamedKey::F19 => Some(33),
        NamedKey::F20 => Some(34),
        _ => None,
    }
}

/// Build a CSI letter sequence: `\x1b[1;{param}{letter}` or `\x1b[{letter}`.
fn build_csi_letter_seq(letter: u8, modifier_param: Option<u8>) -> Vec<u8> {
    modifier_param.map_or_else(
        || vec![0x1b, b'[', letter],
        |param| {
            let mut seq = Vec::with_capacity(8);
            seq.extend_from_slice(b"\x1b[1;");
            seq.extend_from_slice(param.to_string().as_bytes());
            seq.push(letter);
            seq
        },
    )
}

/// Build a CSI tilde sequence: `\x1b[{code};{param}~` or `\x1b[{code}~`.
fn build_csi_tilde_seq(code: u8, modifier_param: Option<u8>) -> Vec<u8> {
    let mut seq = Vec::with_capacity(10);
    seq.extend_from_slice(b"\x1b[");
    seq.extend_from_slice(code.to_string().as_bytes());
    if let Some(param) = modifier_param {
        seq.push(b';');
        seq.extend_from_slice(param.to_string().as_bytes());
    }
    seq.push(b'~');
    seq
}

/// Build a conformant Kitty CSI-u key sequence.
///
/// Forms (per the published Kitty keyboard protocol):
/// - `CSI <cp> u` — no modifiers, no event type;
/// - `CSI <cp> ; <mods> u` — modifiers present, press (or event type omitted);
/// - `CSI <cp> ; <mods> : <event-type> u` — modifiers and a non-press event;
/// - `CSI <cp> ; 1 : <event-type> u` — event type without held modifiers
///   (the modifier sub-field defaults to `1`, as the protocol requires the
///   event type to live in the modifiers parameter's second sub-field).
///
/// `alternates` (`:shifted[:base]`) is appended to the codepoint field and
/// `text_codepoints` becomes the trailing `; <text>` field when present.
fn build_csi_u_seq(
    codepoint: u32,
    alternates: &[u32],
    modifier_param: Option<u8>,
    event_type: Option<u8>,
    text_codepoints: &[u32],
) -> Vec<u8> {
    let mut seq = Vec::with_capacity(16);
    seq.extend_from_slice(b"\x1b[");
    seq.extend_from_slice(codepoint.to_string().as_bytes());
    for alt in alternates {
        seq.push(b':');
        seq.extend_from_slice(alt.to_string().as_bytes());
    }

    // The modifiers parameter is emitted when modifiers are held OR an event
    // type must be carried (its sub-field). A press event with no modifiers
    // omits the whole parameter.
    if modifier_param.is_some() || event_type.is_some() {
        seq.push(b';');
        seq.extend_from_slice(modifier_param.unwrap_or(1).to_string().as_bytes());
        if let Some(ev) = event_type {
            seq.push(b':');
            seq.extend_from_slice(ev.to_string().as_bytes());
        }
    }

    if !text_codepoints.is_empty() {
        seq.push(b';');
        for (idx, cp) in text_codepoints.iter().enumerate() {
            if idx > 0 {
                seq.push(b':');
            }
            seq.extend_from_slice(cp.to_string().as_bytes());
        }
    }

    seq.push(b'u');
    seq
}

// ---------------------------------------------------------------------------
// Kitty keyboard protocol (CSI-u) encoding
//
// Active only when at least one enhancement flag is negotiated. Faithful to
// the published Kitty keyboard protocol; only negotiated flags affect output.
// ---------------------------------------------------------------------------

/// Kitty event type carried in the modifiers parameter's second sub-field.
///
/// `1` (press) is represented as `None` so it is omitted from the wire form,
/// matching the protocol. `2` = repeat, `3` = release.
fn kitty_event_type(event: &KeyEvent, flags: KittyFlags) -> Option<u8> {
    if !flags.report_event_types() {
        return None;
    }
    match event.state {
        ElementState::Released => Some(3),
        ElementState::Pressed if event.repeat => Some(2),
        ElementState::Pressed => None,
    }
}

/// Level-4 Kitty translation entry point.
///
/// Unlike the legacy path this does **not** gate on `ElementState::Pressed`:
/// release/repeat events are encoded when `report_event_types` is negotiated
/// (T013). When `report_event_types` is off, release events still produce no
/// bytes because non-press events only matter to event-type reporting.
fn translate_key_kitty(
    event: &KeyEvent,
    modifiers: ModifiersState,
    flags: KittyFlags,
) -> Option<Vec<u8>> {
    // Without event-type reporting, only key presses generate bytes — a
    // release/repeat is indistinguishable from a press on the wire, so
    // emitting it would double-send the key.
    if !flags.report_event_types() && event.state != ElementState::Pressed {
        return None;
    }

    // Drop Super-only combos that matched no binding — on macOS these are
    // OS-level shortcuts; sending them to the PTY would be wrong. Super
    // combined with another modifier is still encoded (Super is a valid
    // Kitty modifier), mirroring the legacy guard's intent.
    if modifiers.super_key()
        && !modifiers.control_key()
        && !modifiers.alt_key()
        && !modifiers.shift_key()
    {
        return None;
    }

    match &event.logical_key {
        Key::Character(c) => translate_character_kitty(event, c, modifiers, flags),
        Key::Named(named) => translate_named_kitty(event, *named, modifiers, flags),
        _ => None,
    }
}

/// Encode a character key as a Kitty CSI-u sequence (or plain text when the
/// protocol permits it).
fn translate_character_kitty(
    event: &KeyEvent,
    logical: &str,
    modifiers: ModifiersState,
    flags: KittyFlags,
) -> Option<Vec<u8>> {
    let base = base_codepoint_for_character(event, logical)?;
    let modifier_param = xterm_modifier_param(modifiers);
    let event_type = kitty_event_type(event, flags);

    // With only `disambiguate` (and not `report_all_keys`), an unmodified
    // printable key that produces text is sent as its text — exactly as
    // legacy — so plain typing is unchanged. Any held modifier (other than a
    // lone Shift that just selects the shifted glyph) forces CSI-u so that
    // e.g. Ctrl+I is distinct from Tab.
    let has_forcing_modifier =
        modifiers.control_key() || modifiers.alt_key() || modifiers.super_key();
    let needs_csi = flags.report_all_keys()
        || has_forcing_modifier
        || event_type.is_some()
        || (flags.report_alternate_keys() && modifiers.shift_key());

    if !needs_csi {
        // Unmodified (or Shift-only) printable key: send associated text,
        // identical to legacy output.
        return event
            .text
            .as_ref()
            .map(|t| t.as_bytes().to_vec())
            .or_else(|| Some(logical.as_bytes().to_vec()));
    }

    let alternates = alternate_codepoints(logical, base, flags);
    let text_codepoints = associated_text_codepoints(event, flags);
    Some(build_csi_u_seq(base, &alternates, modifier_param, event_type, &text_codepoints))
}

/// Encode a named key as a Kitty CSI-u sequence, falling back to the legacy
/// named-key forms only when no enhancement requires CSI-u for that key.
fn translate_named_kitty(
    event: &KeyEvent,
    named: NamedKey,
    modifiers: ModifiersState,
    flags: KittyFlags,
) -> Option<Vec<u8>> {
    let modifier_param = xterm_modifier_param(modifiers);
    let event_type = kitty_event_type(event, flags);

    if let Some(cp) = kitty_functional_codepoint(named, event.location) {
        // Modifier/lock keys are reported only when the application asked for
        // all keys (or event types) — otherwise they are swallowed as today.
        if is_kitty_modifier_codepoint(cp)
            && !flags.report_all_keys()
            && !flags.report_event_types()
        {
            return None;
        }

        // The text-like specials Enter/Tab/Esc/Backspace are emitted as CSI-u
        // under `disambiguate` per the protocol's functional-key table. Space
        // is plain text: unmodified Space stays a raw 0x20 (legacy) and only
        // becomes CSI-u when modified, event-typed, or under
        // `report_all_keys`. Arrows and the rest also require modifiers, an
        // event type, or `report_all_keys`.
        let always_csi = is_kitty_text_special(cp) && flags.disambiguate();
        let needs_csi = always_csi
            || flags.report_all_keys()
            || modifier_param.is_some()
            || event_type.is_some()
            || is_kitty_modifier_codepoint(cp);

        if needs_csi {
            let text_codepoints = associated_text_codepoints(event, flags);
            return Some(build_csi_u_seq(cp, &[], modifier_param, event_type, &text_codepoints));
        }
    }

    // No enhancement forces CSI-u for this key: fall back to the exact legacy
    // named-key encoding (press-only, like before).
    if event.state != ElementState::Pressed {
        return None;
    }
    translate_named_legacy(named, modifiers)
}

/// Resolve the unshifted base Unicode codepoint for a character key.
///
/// Prefers the platform `key_without_modifiers()` (the layout's base key);
/// where that is unavailable or non-character it degrades to the lowercased
/// logical character (FR per research D-note: platform-gated API must degrade
/// gracefully).
fn base_codepoint_for_character(event: &KeyEvent, logical: &str) -> Option<u32> {
    if let Key::Character(base) = event.key_without_modifiers() {
        if let Some(ch) = base.chars().next() {
            return Some(u32::from(ch));
        }
    }
    let ch = logical.chars().next()?;
    // Lowercase ASCII so the reported key is layout-base, not the shifted
    // glyph (Kitty reports the unshifted key; Shift is in the modifiers).
    Some(u32::from(ch.to_ascii_lowercase()))
}

/// Build the `:shifted` alternate-codepoint list when `report_alternate_keys`
/// is negotiated and the shifted glyph differs from the base key, else empty.
///
/// The primary codepoint already carries the base (unshifted) key, so only
/// the shifted sub-field adds information here. A distinct base-layout
/// sub-field is intentionally not emitted: winit exposes only the active
/// layout's base (`key_without_modifiers`) and the logical glyph — there is
/// no separate standard-layout mapping to report, and the protocol requires
/// that absent fields simply be omitted.
fn alternate_codepoints(logical: &str, base: u32, flags: KittyFlags) -> Vec<u32> {
    if !flags.report_alternate_keys() {
        return Vec::new();
    }
    let shifted = logical.chars().next().map_or(0, u32::from);
    if shifted == 0 || shifted == base {
        return Vec::new();
    }
    vec![shifted]
}

/// Unicode scalar values of the event's associated text, when
/// `report_associated_text` is negotiated and the event carries text.
///
/// Release events carry no associated text.
fn associated_text_codepoints(event: &KeyEvent, flags: KittyFlags) -> Vec<u32> {
    if !flags.report_associated_text() || event.state == ElementState::Released {
        return Vec::new();
    }
    event.text.as_ref().map(|t| t.chars().map(u32::from).collect()).unwrap_or_default()
}

/// Map a `NamedKey` to its Kitty functional-key codepoint.
///
/// Covers the protocol's non-modifier functional keys plus the modifier/lock
/// keys (disambiguated left/right via `KeyLocation`). Returns `None` for keys
/// that have no dedicated functional number (those use the legacy forms).
fn kitty_functional_codepoint(named: NamedKey, location: KeyLocation) -> Option<u32> {
    if let Some(cp) = kitty_modifier_lock_codepoint(named, location) {
        return Some(cp);
    }
    let cp = match named {
        NamedKey::Escape => 27,
        NamedKey::Enter => 13,
        NamedKey::Tab => 9,
        NamedKey::Backspace => 127,
        NamedKey::Space => 32,
        NamedKey::Insert => 57348,
        NamedKey::Delete => 57349,
        NamedKey::ArrowLeft => 57350,
        NamedKey::ArrowRight => 57351,
        NamedKey::ArrowUp => 57352,
        NamedKey::ArrowDown => 57353,
        NamedKey::PageUp => 57354,
        NamedKey::PageDown => 57355,
        NamedKey::Home => 57356,
        NamedKey::End => 57357,
        NamedKey::F1 => 57364,
        NamedKey::F2 => 57365,
        NamedKey::F3 => 57366,
        NamedKey::F4 => 57367,
        NamedKey::F5 => 57368,
        NamedKey::F6 => 57369,
        NamedKey::F7 => 57370,
        NamedKey::F8 => 57371,
        NamedKey::F9 => 57372,
        NamedKey::F10 => 57373,
        NamedKey::F11 => 57374,
        NamedKey::F12 => 57375,
        NamedKey::F13 => 57376,
        NamedKey::F14 => 57377,
        NamedKey::F15 => 57378,
        NamedKey::F16 => 57379,
        NamedKey::F17 => 57380,
        NamedKey::F18 => 57381,
        NamedKey::F19 => 57382,
        NamedKey::F20 => 57383,
        _ => return None,
    };
    Some(cp)
}

/// Modifier/lock key codepoints, with left/right disambiguation by location.
fn kitty_modifier_lock_codepoint(named: NamedKey, location: KeyLocation) -> Option<u32> {
    let is_right = location == KeyLocation::Right;
    let cp = match named {
        NamedKey::Shift => {
            if is_right {
                57447
            } else {
                57441
            }
        }
        NamedKey::Control => {
            if is_right {
                57448
            } else {
                57442
            }
        }
        NamedKey::Alt => {
            if is_right {
                57449
            } else {
                57443
            }
        }
        NamedKey::Super => {
            if is_right {
                57450
            } else {
                57444
            }
        }
        NamedKey::CapsLock => 57358,
        NamedKey::NumLock => 57360,
        _ => return None,
    };
    Some(cp)
}

/// `true` for the text-like specials that emit CSI-u under `disambiguate`
/// alone (Esc, Enter, Tab, Backspace). Space (32) is deliberately excluded:
/// it is plain text and only escalates to CSI-u when modified, event-typed,
/// or under `report_all_keys`.
const fn is_kitty_text_special(codepoint: u32) -> bool {
    matches!(codepoint, 27 | 13 | 9 | 127)
}

/// `true` for the Kitty modifier/lock-key private-use codepoints.
const fn is_kitty_modifier_codepoint(codepoint: u32) -> bool {
    matches!(
        codepoint,
        57441 | 57447 | 57442 | 57448 | 57443 | 57449 | 57444 | 57450 | 57358 | 57360
    )
}
