//! Keybinding parser and layout-action dispatch for the GPUI client.
//!
//! This is a port of the winit-driven keybinding system in
//! `crates/scribe-client/src/input.rs`, retargeted at GPUI's
//! [`gpui::Keystroke`] / [`gpui::Modifiers`] model via the intermediate
//! [`KeyInput`] the terminal byte encoder already defines. It reproduces the
//! full [`Bindings`] parser (every configurable action from
//! [`scribe_common::config::KeybindingsConfig`]), all [`LayoutAction`] and
//! [`KeyAction`] variants named in the parity inventory, and the three-level
//! dispatch order the legacy client used: layout shortcuts, then palette /
//! settings / find, then fixed terminal-shortcut escape sequences, before the
//! generic byte encoder handles the key.
//!
//! The 50+ actions are enumerated exhaustively so no user shortcut regresses
//! across the cutover: the pane/workspace/tab/navigation/view layout tables and
//! the terminal-shortcut table are each ported one-for-one.

use gpui::Modifiers;
use scribe_common::config::{KeyComboList, KeybindingsConfig};

use crate::input::{KeyInput, KeyToken, NamedKey};

/// A set of parsed keybindings for a single action (one or more combos).
pub type BindingSet = Vec<Keybinding>;

/// Returns `true` if any binding in `set` matches the key event.
#[must_use]
pub fn any_matches(set: &BindingSet, input: &KeyInput) -> bool {
    set.iter().any(|b| b.matches(input))
}

/// A parsed key match target: either a single character or a named key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMatch {
    /// A single character key (e.g. `'w'`, `'\\'`, `'-'`).
    Character(char),
    /// A named key (e.g. `Tab`, `Enter`).
    Named(NamedKey),
}

/// A parsed keybinding: required modifier state + key target.
#[derive(Debug, Clone, Copy)]
pub struct Keybinding {
    /// The exact modifier state required for this keybinding. Only the
    /// control/alt/shift/platform flags participate; the function flag is
    /// ignored so it never blocks an otherwise-matching chord.
    pub modifiers: Modifiers,
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
    pub equalize: BindingSet,

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
    pub new_pi_tab: BindingSet,
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

    // Terminal shortcuts
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
    /// Returns `None` if the string is malformed or the key part is
    /// unrecognised. The modifier vocabulary matches the legacy client:
    /// `ctrl`, `shift`, `alt`, and `cmd`/`super` (mapped to GPUI's platform
    /// modifier).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = Modifiers::default();
        let mut key_part: Option<String> = None;

        for part in s.split('+') {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" => modifiers.control = true,
                "shift" => modifiers.shift = true,
                "alt" => modifiers.alt = true,
                "cmd" | "super" => modifiers.platform = true,
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
            ch if ch.chars().count() == 1 => KeyMatch::Character(ch.chars().next()?),
            _ => return None,
        };

        Some(Self { modifiers, key })
    }

    /// Returns `true` if `input` matches this keybinding.
    ///
    /// Only key-down (press or auto-repeat) events can match; the modifier
    /// state must match exactly on the four real modifier flags (the GPUI
    /// function flag is ignored). Character matches compare against the
    /// unshifted base character case-insensitively, mirroring winit's
    /// `key_without_modifiers`, so `ctrl+w` fires regardless of caps state.
    #[must_use]
    pub fn matches(&self, input: &KeyInput) -> bool {
        if !input.is_down() {
            return false;
        }
        match self.key {
            KeyMatch::Character(c) => self.character_matches(c, input),
            KeyMatch::Named(named) => {
                modifiers_match(self.modifiers, input.modifiers)
                    && input.token == KeyToken::Named(named)
            }
        }
    }

    /// Match a character binding, allowing for GPUI's shifted-symbol spelling.
    ///
    /// GPUI can spell shifted symbols in two different ways. Linux backends
    /// often resolve the keysym at the active modifier level and drop the shift
    /// flag for single-character non-letter keys, so pressing `ctrl+shift+\`
    /// arrives as control plus the key `|` with shift clear. macOS can keep
    /// the shift flag *and* still report the shifted glyph (`}` for
    /// `cmd+shift+]`). Every shifted-symbol default binding —
    /// `split_vertical`, `split_horizontal`, `zoom_in`, `next_tab`, and
    /// `prev_tab` — is unreachable without accepting both spellings, so a
    /// shift-carrying character binding also matches its US-layout shifted
    /// glyph whether GPUI folded shift away or preserved it.
    fn character_matches(&self, target: char, input: &KeyInput) -> bool {
        let Some(base) = input.base else {
            return false;
        };
        if modifiers_match(self.modifiers, input.modifiers) {
            if base.eq_ignore_ascii_case(&target) {
                return true;
            }
            if self.modifiers.shift && shifted_ascii(target).is_some_and(|shifted| base == shifted)
            {
                return true;
            }
        }
        if !self.modifiers.shift || input.modifiers.shift {
            return false;
        }
        let Some(shifted) = shifted_ascii(target) else {
            return false;
        };
        let folded = Modifiers { shift: false, ..self.modifiers };
        modifiers_match(folded, input.modifiers) && base == shifted
    }
}

/// The glyph a US-layout key produces with Shift held, for the keys whose
/// shifted form is a distinct symbol.
///
/// Letters are absent on purpose: GPUI already reports them by their own
/// lowercase key and keeps the shift flag, so they match the ordinary way.
const fn shifted_ascii(key: char) -> Option<char> {
    let shifted = match key {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => return None,
    };
    Some(shifted)
}

/// Compare two modifier states on the four real modifiers, ignoring the GPUI
/// function flag (which no keybinding string can request).
fn modifiers_match(required: Modifiers, actual: Modifiers) -> bool {
    required.control == actual.control
        && required.alt == actual.alt
        && required.shift == actual.shift
        && required.platform == actual.platform
}

impl Bindings {
    /// Parse all keybindings from config.
    ///
    /// Defaults are defined in [`KeybindingsConfig::default()`] (the single
    /// source of truth). Serde fills them in for any missing config fields, so
    /// every list is non-empty by the time it reaches here. Invalid entries are
    /// skipped with a warning.
    #[must_use]
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
            equalize: parse_set(&config.equalize),

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
            new_pi_tab: parse_set(&config.new_pi_tab),
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
/// Defaults are provided by [`KeybindingsConfig::default()`] via serde, so the
/// list is always populated for well-formed configs.
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
    /// Reset every workspace-region and pane split to equal space.
    Equalize,

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
    /// Open a new tab running Pi in the focused workspace.
    NewPiTab,
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

/// A shell-owned overlay the client opens from a fixed, non-configurable chord.
///
/// These are the surfaces that have no `KeybindingsConfig` field yet, so the
/// shell hard-codes a chord for them. Because they are not in the binding
/// tables, nothing stops a hard-coded chord from colliding with a user-facing
/// action — which is exactly how the Linux `close_tab` default (`ctrl+shift+q`)
/// became unreachable. [`translate_overlay_chord`] resolves that collision in
/// one place, in favour of the configured binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayChord {
    /// Toggle the tooltip demo overlay.
    TooltipDemo,
    /// Open the close-confirmation dialog.
    CloseDialog,
    /// Open the clipboard-confirmation dialog.
    ClipboardDialog,
    /// Toggle vi / copy mode over the terminal grid.
    ///
    /// Vi mode is a shell-owned keyboard mode rather than a painted overlay,
    /// but it lands here for the same reason the surfaces above do:
    /// [`KeybindingsConfig`] has no field for it, so the shell has to name a
    /// chord, and naming it here is what makes it yield to a user rebind that
    /// happens to land on the same keys.
    ViMode,
}

/// The fixed chord for each [`OverlayChord`].
///
/// Every entry is deliberately kept off the default binding table so the
/// overlays stay reachable out of the box: `ctrl+shift+q` (`close_tab`) and
/// `ctrl+shift+n` (`new_window`) are user actions and are therefore NOT used
/// here. A rebind can still move a user action onto one of these chords, which
/// [`translate_overlay_chord`] resolves in the user's favour.
pub const OVERLAY_CHORDS: [(&str, OverlayChord); 4] = [
    ("ctrl+shift+u", OverlayChord::TooltipDemo),
    ("ctrl+shift+d", OverlayChord::CloseDialog),
    ("ctrl+shift+k", OverlayChord::ClipboardDialog),
    ("ctrl+shift+space", OverlayChord::ViMode),
];

/// Match a key event against the shell-owned overlay chords.
///
/// A configured binding always wins: when `input` resolves to any
/// [`KeyAction`], this returns `None` so the caller falls through to the
/// binding dispatcher instead of swallowing the keystroke. That precedence is
/// what keeps every bound action reachable no matter which chord the shell
/// hard-codes, and it survives a rebind in either direction.
#[must_use]
pub fn translate_overlay_chord(input: &KeyInput, bindings: &Bindings) -> Option<OverlayChord> {
    if !input.is_down() {
        return None;
    }
    if translate_key_action(input, bindings).is_some() {
        return None;
    }
    OVERLAY_CHORDS.iter().find_map(|(combo, chord)| {
        Keybinding::parse(combo).is_some_and(|binding| binding.matches(input)).then_some(*chord)
    })
}

/// Result of translating a key event against the bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Match a key event against the binding tables, returning the intercepted
/// non-terminal action if one fires.
///
/// This covers levels 1–3 of the legacy dispatch order (layout shortcuts,
/// palette/settings/find, fixed terminal-shortcut escape sequences). The caller
/// runs its generic terminal byte encoder for level 4 when this returns `None`.
/// All intercepts are press/repeat gated, matching the legacy `Pressed` gate.
#[must_use]
pub fn translate_key_action(input: &KeyInput, bindings: &Bindings) -> Option<KeyAction> {
    if !input.is_down() {
        return None;
    }

    if let Some(action) = translate_layout_shortcut(input, bindings) {
        return Some(KeyAction::Layout(action));
    }

    if any_matches(&bindings.command_palette, input) {
        return Some(KeyAction::OpenCommandPalette);
    }

    if any_matches(&bindings.settings, input) {
        return Some(KeyAction::OpenSettings);
    }

    if any_matches(&bindings.find, input) {
        return Some(KeyAction::OpenFind);
    }

    translate_terminal_shortcut(input, bindings).map(KeyAction::Terminal)
}

/// Check for layout shortcuts using the provided bindings.
#[must_use]
pub fn translate_layout_shortcut(input: &KeyInput, bindings: &Bindings) -> Option<LayoutAction> {
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
    .find_map(|actions| match_binding_actions(input, actions))
}

/// Check configurable terminal shortcut bindings.
///
/// Each binding maps a key combination to a fixed escape sequence sent to the
/// PTY.
#[must_use]
pub fn translate_terminal_shortcut(input: &KeyInput, bindings: &Bindings) -> Option<Vec<u8>> {
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

    shortcuts
        .iter()
        .find_map(|entry| any_matches(entry.bindings, input).then(|| entry.action.to_vec()))
}

struct BindingAction<'a, T> {
    bindings: &'a BindingSet,
    action: T,
}

fn pane_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 9] {
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
        BindingAction { bindings: &bindings.equalize, action: LayoutAction::Equalize },
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

fn tab_layout_actions(bindings: &Bindings) -> [BindingAction<'_, LayoutAction>; 19] {
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
        BindingAction { bindings: &bindings.new_pi_tab, action: LayoutAction::NewPiTab },
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
    input: &KeyInput,
    candidates: &[BindingAction<'_, T>],
) -> Option<T> {
    candidates.iter().find_map(|entry| any_matches(entry.bindings, input).then_some(entry.action))
}

#[cfg(test)]
mod tests;
