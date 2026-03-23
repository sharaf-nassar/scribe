//! Winit key event translation for terminal input and layout commands.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Layout commands intercepted before normal key translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
    /// Split the focused pane vertically (side-by-side).
    SplitVertical,
    /// Split the focused pane horizontally (top/bottom).
    SplitHorizontal,
    /// Close the focused pane.
    ClosePane,
    /// Cycle focus to the next pane.
    FocusNext,
}

/// Result of translating a winit key event.
#[derive(Debug)]
pub enum KeyAction {
    /// Terminal byte sequence to send to the PTY.
    Terminal(Vec<u8>),
    /// Layout command (split, close, focus).
    Layout(LayoutAction),
    /// Open the settings window.
    OpenSettings,
}

/// Translate a winit key event into either terminal bytes or a layout command.
///
/// Layout shortcuts (Ctrl+Shift combinations) are intercepted first.
/// Returns `None` if the key should be ignored.
pub fn translate_key_action(event: &KeyEvent, modifiers: ModifiersState) -> Option<KeyAction> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Check for layout shortcuts first (Ctrl+Shift combos).
    if let Some(action) = translate_layout_shortcut(event, modifiers) {
        return Some(KeyAction::Layout(action));
    }

    // Ctrl+, opens settings.
    if modifiers.control_key() && !modifiers.shift_key() {
        if let Key::Character(c) = &event.logical_key {
            if c.as_str() == "," {
                return Some(KeyAction::OpenSettings);
            }
        }
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

/// Check for Ctrl+Shift layout shortcuts.
///
/// - `Ctrl+Shift+\` -- split vertical (side-by-side)
/// - `Ctrl+Shift+-` -- split horizontal (top/bottom)
/// - `Ctrl+Shift+W` -- close pane
/// - `Ctrl+Tab` -- focus next pane
fn translate_layout_shortcut(event: &KeyEvent, modifiers: ModifiersState) -> Option<LayoutAction> {
    let ctrl_shift = modifiers.control_key() && modifiers.shift_key();
    let ctrl = modifiers.control_key();

    if ctrl_shift {
        if let Key::Character(c) = &event.logical_key {
            let ch = c.chars().next()?;
            match ch {
                '\\' | '|' => return Some(LayoutAction::SplitVertical),
                '-' | '_' => return Some(LayoutAction::SplitHorizontal),
                'W' | 'w' => return Some(LayoutAction::ClosePane),
                _ => {}
            }
        }
    }

    // Ctrl+Tab (no Shift required).
    if ctrl {
        if let Key::Named(NamedKey::Tab) = &event.logical_key {
            return Some(LayoutAction::FocusNext);
        }
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
