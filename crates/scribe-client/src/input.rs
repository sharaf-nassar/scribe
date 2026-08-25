//! Terminal key-input byte encoder for the GPUI client.
//!
//! This is a byte-for-byte port of the winit-driven encoder in
//! `crates/scribe-client/src/input.rs`, retargeted at GPUI's
//! [`gpui::KeyDownEvent`] / [`gpui::Keystroke`] model. It reproduces the four
//! Kitty progressive-enhancement flags, the CSI-u functional-key table, the
//! legacy xterm modifier encoding, the DECCKM / DECPAM application modes, and
//! the numpad SS3 table. The golden fixtures in
//! `tests/fixtures/gpui-client/keyboard-byte-golden.json` are the correctness
//! oracle (US1): every encoder path must stay byte-identical to the old client.
//!
//! GPUI's [`gpui::Keystroke`] cannot express two facts the winit encoder relied
//! on: numeric-keypad location (KP keysyms are folded into their base names)
//! and a distinct unshifted base vs shifted glyph. The encoder therefore
//! operates on an intermediate [`KeyInput`] that carries these fields
//! explicitly; [`KeyInput::from_key_down`] populates what GPUI can supply, and
//! callers with richer platform data can fill `numpad` / `base` directly.

use gpui::{KeyDownEvent, KeyUpEvent, Keystroke, Modifiers};

/// The named (non-character) keys the terminal encoder distinguishes.
///
/// This is the local analogue of winit's `NamedKey`, narrowed to the variants
/// the encoder tables reference. Character keys travel through
/// [`KeyToken::Char`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    Shift,
    Control,
    Alt,
    Super,
    CapsLock,
    NumLock,
}

/// Physical location of a key, used for DECPAM numpad routing and left/right
/// modifier disambiguation.
///
/// Mirrors the subset of winit's `KeyLocation` the encoder needs. `Left` folds
/// into `Standard` because the encoder only disambiguates the right-hand
/// instance of modifier/lock keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyLocation {
    /// A main-section (or left-hand) key.
    #[default]
    Standard,
    /// A numeric-keypad key (DECPAM SS3 table).
    Numpad,
    /// The right-hand instance of a modifier/lock key.
    Right,
}

/// The key identity carried by a [`KeyInput`]: either a logical character or a
/// named key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyToken {
    /// A logical character (the shifted glyph, e.g. `'A'` for Shift+A).
    Char(char),
    /// A named key (e.g. [`NamedKey::Enter`]).
    Named(NamedKey),
}

/// The press/repeat/release state of a key event.
///
/// winit modelled auto-repeat as a `Pressed` event with `repeat = true`; the
/// encoder needs to tell press from repeat (Kitty event-type reporting) and
/// both from release, so the three cases are explicit here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Initial key press.
    Pressed,
    /// Auto-repeat while the key is held.
    Repeat,
    /// Key release.
    Released,
}

/// A normalized key event consumed by the terminal byte encoder.
///
/// Bundles the logical key, the unshifted base character (for Kitty base-layout
/// codepoints), the associated text, the modifier state, keypad/right-side
/// location bits, and the press/repeat/release state. GPUI events are lowered
/// into this shape by [`KeyInput::from_key_down`] / [`KeyInput::from_key_up`].
#[derive(Debug, Clone)]
pub struct KeyInput {
    /// The logical key that was pressed.
    pub token: KeyToken,
    /// The unshifted base character, when known (winit's
    /// `key_without_modifiers`). Used to derive the Kitty base codepoint.
    pub base: Option<char>,
    /// The associated text produced by the event, when any.
    pub text: Option<String>,
    /// The modifier state at the time of the event.
    pub modifiers: Modifiers,
    /// The physical location of the key (numpad / right-hand disambiguation).
    pub location: KeyLocation,
    /// The press/repeat/release state.
    pub state: KeyState,
}

impl KeyInput {
    /// `true` for press and repeat events (winit's `ElementState::Pressed`).
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.state != KeyState::Released
    }

    /// `true` for auto-repeat events.
    #[must_use]
    pub fn is_repeat(&self) -> bool {
        self.state == KeyState::Repeat
    }

    /// Lower a GPUI [`gpui::KeyDownEvent`] into a [`KeyInput`].
    ///
    /// Returns `None` when the keystroke names no key the encoder handles.
    /// `is_held` selects [`KeyState::Repeat`]; otherwise the event is a press.
    /// Numpad location is unavailable from [`gpui::Keystroke`], so `numpad` is
    /// always `false` on this path.
    #[must_use]
    pub fn from_key_down(event: &KeyDownEvent) -> Option<Self> {
        let state = if event.is_held { KeyState::Repeat } else { KeyState::Pressed };
        Self::from_keystroke(&event.keystroke, state)
    }

    /// Lower a GPUI [`gpui::KeyUpEvent`] into a released [`KeyInput`].
    #[must_use]
    pub fn from_key_up(event: &KeyUpEvent) -> Option<Self> {
        Self::from_keystroke(&event.keystroke, KeyState::Released)
    }

    fn from_keystroke(keystroke: &Keystroke, state: KeyState) -> Option<Self> {
        let token = token_from_key_name(&keystroke.key)?;
        // `keystroke.key` is the layout base (e.g. "a" for Shift+A); `key_char`
        // is the typed glyph (e.g. "A"). The logical token prefers the typed
        // glyph so the shifted alternate codepoint is available to Kitty.
        let base = {
            let mut chars = keystroke.key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        let token = match (token, keystroke.key_char.as_deref()) {
            (KeyToken::Char(_), Some(text)) => text.chars().next().map_or(token, KeyToken::Char),
            _ => token,
        };
        Some(Self {
            token,
            base,
            text: keystroke.key_char.clone(),
            modifiers: keystroke.modifiers,
            location: KeyLocation::Standard,
            state,
        })
    }
}

/// Map a GPUI keystroke key name to a [`KeyToken`].
fn token_from_key_name(key: &str) -> Option<KeyToken> {
    let named = match key {
        "enter" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "escape" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "space" => NamedKey::Space,
        "up" => NamedKey::ArrowUp,
        "down" => NamedKey::ArrowDown,
        "left" => NamedKey::ArrowLeft,
        "right" => NamedKey::ArrowRight,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "insert" => NamedKey::Insert,
        "delete" => NamedKey::Delete,
        "pageup" => NamedKey::PageUp,
        "pagedown" => NamedKey::PageDown,
        "f1" => NamedKey::F1,
        "f2" => NamedKey::F2,
        "f3" => NamedKey::F3,
        "f4" => NamedKey::F4,
        "f5" => NamedKey::F5,
        "f6" => NamedKey::F6,
        "f7" => NamedKey::F7,
        "f8" => NamedKey::F8,
        "f9" => NamedKey::F9,
        "f10" => NamedKey::F10,
        "f11" => NamedKey::F11,
        "f12" => NamedKey::F12,
        "f13" => NamedKey::F13,
        "f14" => NamedKey::F14,
        "f15" => NamedKey::F15,
        "f16" => NamedKey::F16,
        "f17" => NamedKey::F17,
        "f18" => NamedKey::F18,
        "f19" => NamedKey::F19,
        "f20" => NamedKey::F20,
        "shift" => NamedKey::Shift,
        "control" | "ctrl" => NamedKey::Control,
        "alt" => NamedKey::Alt,
        "super" | "cmd" | "platform" | "win" => NamedKey::Super,
        "capslock" => NamedKey::CapsLock,
        "numlock" => NamedKey::NumLock,
        other => {
            let mut chars = other.chars();
            let first = chars.next()?;
            if chars.next().is_none() {
                return Some(KeyToken::Char(first));
            }
            return None;
        }
    };
    Some(KeyToken::Named(named))
}

/// The set of Kitty keyboard-protocol progressive-enhancement flags currently
/// negotiated by the focused terminal application.
///
/// Models the five independent flags of the Kitty keyboard protocol
/// (`disambiguate escape codes`, `report event types`, `report alternate
/// keys`, `report all keys as escape codes`, `report associated text`). Stored
/// as a bitfield rather than five `bool` fields to satisfy the workspace
/// `clippy::struct_excessive_bools` gate.
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

    /// All flags off — pure legacy encoding.
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
    #[must_use]
    pub const fn legacy(self) -> bool {
        self.bits == 0
    }
}

/// Per-pane terminal protocol state consulted by the encoder.
///
/// Bundles the negotiated Kitty flags with the two DEC private modes that
/// change how unmodified cursor and keypad keys are encoded: DECCKM
/// (application cursor keys) and DECPAM (application keypad).
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalMode {
    /// Negotiated Kitty progressive-enhancement flags.
    pub kitty: KittyFlags,
    /// `true` while DECCKM is set (cursor keys send SS3).
    pub app_cursor: bool,
    /// `true` while DECPAM is set (numpad keys send SS3).
    pub app_keypad: bool,
}

impl TerminalMode {
    /// Pure-legacy state: no Kitty flags, no application modes.
    #[must_use]
    pub const fn legacy() -> Self {
        Self { kitty: KittyFlags::legacy_set(), app_cursor: false, app_keypad: false }
    }
}

/// `true` when no relevant modifier (Ctrl/Alt/Shift/Super) is held.
fn mods_empty(m: Modifiers) -> bool {
    !m.control && !m.alt && !m.shift && !m.platform
}

/// Encode a normalized key event into terminal byte sequences.
///
/// DECPAM-gated numpad SS3 forms win first (press-only, unmodified). With no
/// Kitty flag negotiated the legacy xterm encoding is reproduced
/// byte-identically (modulo the DEC private modes); otherwise the Kitty CSI-u
/// encoder runs. Returns `None` when the key should be ignored.
#[must_use]
pub fn encode(input: &KeyInput, mode: TerminalMode) -> Option<Vec<u8>> {
    // DECPAM numpad SS3: press-only, unmodified. Modified numpad chords fall
    // through so the user's modifier reporting is preserved.
    if mode.app_keypad
        && input.is_down()
        && mods_empty(input.modifiers)
        && let Some(bytes) = translate_numpad_app_keypad(input)
    {
        return Some(bytes);
    }

    // Legacy fast path: byte-identical to the pre-feature encoder modulo the
    // DEC modes above.
    if mode.kitty.legacy() {
        if !input.is_down() {
            return None;
        }
        return match input.token {
            KeyToken::Char(c) => translate_character_with_modifiers(c, input.modifiers),
            KeyToken::Named(named) => {
                translate_named_legacy(named, input.modifiers, mode.app_cursor)
            }
        };
    }

    translate_key_kitty(input, mode)
}

// ---------------------------------------------------------------------------
// Legacy (non-Kitty) encoding
// ---------------------------------------------------------------------------

/// Encode a character key with modifier handling (legacy path).
fn translate_character_with_modifiers(c: char, modifiers: Modifiers) -> Option<Vec<u8>> {
    // Drop Cmd/Super combos that matched no binding — on macOS these are
    // OS-level shortcuts and sending raw chars to the PTY would be wrong.
    if modifiers.platform {
        return None;
    }

    let ctrl = modifiers.control;
    let alt = modifiers.alt;

    if ctrl {
        let control_byte = char_to_control_byte(c)?;
        if alt { Some(vec![0x1b, control_byte]) } else { Some(vec![control_byte]) }
    } else if alt {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(char_utf8(c).as_slice());
        Some(bytes)
    } else {
        Some(char_utf8(c))
    }
}

/// UTF-8 bytes of a single character.
fn char_utf8(c: char) -> Vec<u8> {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes().to_vec()
}

/// Convert an ASCII letter to its Ctrl control byte (`0x01`–`0x1a`).
fn char_to_control_byte(c: char) -> Option<u8> {
    let ch = u8::try_from(u32::from(c)).ok()?;
    if ch.is_ascii_lowercase() {
        Some(ch - b'a' + 1)
    } else if ch.is_ascii_uppercase() {
        Some(ch - b'A' + 1)
    } else {
        None
    }
}

/// Legacy named-key translation (byte-identical to the pre-feature path).
fn translate_named_legacy(
    named: NamedKey,
    modifiers: Modifiers,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    if modifiers.platform {
        return None;
    }

    if let Some(bytes) = translate_named_special(named, modifiers) {
        return Some(bytes);
    }

    let modifier_param = xterm_modifier_param(modifiers);

    translate_named_csi_letter(named, modifier_param, app_cursor)
        .or_else(|| translate_named_csi_tilde(named, modifier_param))
        .or_else(|| translate_named_function_key(named, modifier_param))
}

fn translate_named_special(named: NamedKey, modifiers: Modifiers) -> Option<Vec<u8>> {
    match named {
        NamedKey::Backspace => {
            if modifiers.control && modifiers.alt {
                Some(vec![0x1b, 0x08])
            } else if modifiers.alt {
                Some(vec![0x1b, 0x7f])
            } else if modifiers.control {
                Some(vec![0x08])
            } else {
                Some(vec![0x7f])
            }
        }
        NamedKey::Space => {
            if modifiers.control {
                Some(vec![0])
            } else if modifiers.alt {
                Some(vec![0x1b, b' '])
            } else {
                Some(b" ".to_vec())
            }
        }
        NamedKey::Enter => {
            if modifiers.alt {
                Some(vec![0x1b, b'\r'])
            } else {
                Some(b"\r".to_vec())
            }
        }
        NamedKey::Tab => {
            if modifiers.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(b"\t".to_vec())
            }
        }
        NamedKey::Escape => Some(b"\x1b".to_vec()),
        _ => None,
    }
}

fn translate_named_csi_letter(
    named: NamedKey,
    modifier_param: Option<u8>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    translate_named_csi_letter_with_event(named, modifier_param, None, app_cursor)
}

fn translate_named_csi_letter_with_event(
    named: NamedKey,
    modifier_param: Option<u8>,
    event_type: Option<u8>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    csi_letter_for_named(named).map(|letter| {
        // DECCKM: unmodified arrows / Home / End travel as SS3 (`\x1bO<letter>`).
        // Modified chords and Kitty event types force CSI because SS3 carries
        // no modifier or event-type parameter.
        if app_cursor && modifier_param.is_none() && event_type.is_none() {
            vec![0x1b, b'O', letter]
        } else {
            build_csi_letter_seq(letter, modifier_param, event_type)
        }
    })
}

fn translate_named_csi_tilde(named: NamedKey, modifier_param: Option<u8>) -> Option<Vec<u8>> {
    translate_named_csi_tilde_with_event(named, modifier_param, None)
}

fn translate_named_csi_tilde_with_event(
    named: NamedKey,
    modifier_param: Option<u8>,
    event_type: Option<u8>,
) -> Option<Vec<u8>> {
    csi_tilde_code_for_named(named)
        .map(|code| build_csi_tilde_seq(code, modifier_param, event_type))
}

/// Map a numpad key event to its DECPAM SS3 sequence.
///
/// xterm/DEC application-keypad table: digits `0`..`9` map to `\x1bOp`..`\x1bOy`,
/// punctuation `.,-+*/=` to `\x1bOn`/`\x1bOl`/`\x1bOm`/`\x1bOk`/`\x1bOj`/`\x1bOo`/`\x1bOX`,
/// and numpad Enter to `\x1bOM`.
fn translate_numpad_app_keypad(input: &KeyInput) -> Option<Vec<u8>> {
    if input.location != KeyLocation::Numpad {
        return None;
    }
    let letter = match input.token {
        KeyToken::Char(c) => numpad_char_ss3_letter(c)?,
        KeyToken::Named(NamedKey::Enter) => b'M',
        KeyToken::Named(_) => return None,
    };
    Some(vec![0x1b, b'O', letter])
}

fn numpad_char_ss3_letter(c: char) -> Option<u8> {
    let letter = match c {
        '0' => b'p',
        '1' => b'q',
        '2' => b'r',
        '3' => b's',
        '4' => b't',
        '5' => b'u',
        '6' => b'v',
        '7' => b'w',
        '8' => b'x',
        '9' => b'y',
        '.' => b'n',
        ',' => b'l',
        '-' => b'm',
        '+' => b'k',
        '*' => b'j',
        '/' => b'o',
        '=' => b'X',
        _ => return None,
    };
    Some(letter)
}

fn translate_named_function_key(named: NamedKey, modifier_param: Option<u8>) -> Option<Vec<u8>> {
    translate_named_function_key_with_event(named, modifier_param, None)
}

fn translate_named_function_key_with_event(
    named: NamedKey,
    modifier_param: Option<u8>,
    event_type: Option<u8>,
) -> Option<Vec<u8>> {
    ss3_letter_for_fkey(named)
        .map(|letter| {
            if modifier_param.is_none() && event_type.is_none() {
                vec![0x1b, b'O', letter]
            } else {
                build_csi_letter_seq(letter, modifier_param, event_type)
            }
        })
        .or_else(|| {
            fkey_tilde_code(named).map(|code| build_csi_tilde_seq(code, modifier_param, event_type))
        })
}

/// Compute the xterm modifier parameter: `1 + shift(1) + alt(2) + ctrl(4)`.
///
/// Returns `None` when no modifiers are held (the parameter is omitted).
fn xterm_modifier_param(modifiers: Modifiers) -> Option<u8> {
    let mut param: u8 = 1;
    if modifiers.shift {
        param += 1;
    }
    if modifiers.alt {
        param += 2;
    }
    if modifiers.control {
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

/// Build a CSI letter sequence (`\x1b[{letter}`, `\x1b[1;{param}{letter}`, or
/// `\x1b[1;{param}:{event}{letter}`).
fn build_csi_letter_seq(letter: u8, modifier_param: Option<u8>, event_type: Option<u8>) -> Vec<u8> {
    if modifier_param.is_none() && event_type.is_none() {
        return vec![0x1b, b'[', letter];
    }

    let mut seq = Vec::with_capacity(10);
    seq.extend_from_slice(b"\x1b[1;");
    seq.extend_from_slice(modifier_param.unwrap_or(1).to_string().as_bytes());
    if let Some(ev) = event_type {
        seq.push(b':');
        seq.extend_from_slice(ev.to_string().as_bytes());
    }
    seq.push(letter);
    seq
}

/// Build a CSI tilde sequence (`\x1b[{code}~`, `\x1b[{code};{param}~`, or
/// `\x1b[{code};{param}:{event}~`).
fn build_csi_tilde_seq(code: u8, modifier_param: Option<u8>, event_type: Option<u8>) -> Vec<u8> {
    if modifier_param.is_none() && event_type.is_none() {
        let mut seq = Vec::with_capacity(5);
        seq.extend_from_slice(b"\x1b[");
        seq.extend_from_slice(code.to_string().as_bytes());
        seq.push(b'~');
        return seq;
    }

    let mut seq = Vec::with_capacity(12);
    seq.extend_from_slice(b"\x1b[");
    seq.extend_from_slice(code.to_string().as_bytes());
    seq.push(b';');
    seq.extend_from_slice(modifier_param.unwrap_or(1).to_string().as_bytes());
    if let Some(ev) = event_type {
        seq.push(b':');
        seq.extend_from_slice(ev.to_string().as_bytes());
    }
    seq.push(b'~');
    seq
}

/// Build a conformant Kitty CSI-u key sequence.
///
/// Forms: `CSI <cp> u`; `CSI <cp> ; <mods> u`; `CSI <cp> ; <mods> : <event> u`;
/// `CSI <cp> ; 1 : <event> u` (event type without held modifiers). `alternates`
/// is appended to the codepoint field as `:shifted[:base]`, and
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
// ---------------------------------------------------------------------------

/// Kitty event type carried in the modifiers parameter's second sub-field.
///
/// Press (`1`) is `None` so it is omitted; `2` = repeat, `3` = release.
fn kitty_event_type(input: &KeyInput, flags: KittyFlags) -> Option<u8> {
    if !flags.report_event_types() {
        return None;
    }
    match input.state {
        KeyState::Released => Some(3),
        KeyState::Repeat => Some(2),
        KeyState::Pressed => None,
    }
}

/// Level-4 Kitty translation entry point.
fn translate_key_kitty(input: &KeyInput, mode: TerminalMode) -> Option<Vec<u8>> {
    // Without event-type reporting, only presses/repeats generate bytes.
    if !mode.kitty.report_event_types() && !input.is_down() {
        return None;
    }

    // Drop Super-only combos that matched no binding.
    let m = input.modifiers;
    if m.platform && !m.control && !m.alt && !m.shift {
        return None;
    }

    match input.token {
        KeyToken::Char(c) => translate_character_kitty(input, c, m, mode.kitty),
        KeyToken::Named(named) => translate_named_kitty(input, named, m, mode),
    }
}

/// Encode a character key as a Kitty CSI-u sequence (or plain text when the
/// protocol permits it).
fn translate_character_kitty(
    input: &KeyInput,
    logical: char,
    modifiers: Modifiers,
    flags: KittyFlags,
) -> Option<Vec<u8>> {
    let base = base_codepoint_for_character(input, logical);
    let modifier_param = xterm_modifier_param(modifiers);
    let event_type = kitty_event_type(input, flags);

    // Any forcing modifier (Ctrl/Alt/Super), an event type, or shifted
    // alternate reporting forces CSI-u so Ctrl+I stays distinct from Tab.
    let has_forcing_modifier = modifiers.control || modifiers.alt || modifiers.platform;
    let needs_csi = flags.report_all_keys()
        || has_forcing_modifier
        || event_type.is_some()
        || (flags.report_alternate_keys() && modifiers.shift);

    if !needs_csi {
        // Unmodified (or Shift-only) printable key: send associated text,
        // identical to legacy output.
        return input
            .text
            .as_ref()
            .map(|t| t.as_bytes().to_vec())
            .or_else(|| Some(char_utf8(logical)));
    }

    let alternates = alternate_codepoints(logical, base, flags);
    let text_codepoints = associated_text_codepoints(input, flags);
    Some(build_csi_u_seq(base, &alternates, modifier_param, event_type, &text_codepoints))
}

/// Encode a named key as a Kitty CSI-u sequence, falling back to the legacy
/// named-key forms when no enhancement requires CSI-u for that key.
fn translate_named_kitty(
    input: &KeyInput,
    named: NamedKey,
    modifiers: Modifiers,
    mode: TerminalMode,
) -> Option<Vec<u8>> {
    let event_type = kitty_event_type(input, mode.kitty);
    let text_codepoints = associated_text_codepoints(input, mode.kitty);
    translate_named_kitty_fields(NamedKittyFields {
        named,
        location: input.location,
        modifiers,
        flags: mode.kitty,
        app_cursor: mode.app_cursor,
        event_type,
        text_codepoints: &text_codepoints,
        pressed: input.is_down(),
    })
}

#[derive(Clone, Copy)]
struct NamedKittyFields<'a> {
    named: NamedKey,
    location: KeyLocation,
    modifiers: Modifiers,
    flags: KittyFlags,
    app_cursor: bool,
    event_type: Option<u8>,
    text_codepoints: &'a [u32],
    pressed: bool,
}

fn translate_named_kitty_fields(request: NamedKittyFields<'_>) -> Option<Vec<u8>> {
    let NamedKittyFields {
        named,
        location,
        modifiers,
        flags,
        app_cursor,
        event_type,
        text_codepoints,
        pressed,
    } = request;
    let modifier_param = xterm_modifier_param(modifiers);

    if event_type.is_some()
        && let Some(bytes) =
            translate_named_kitty_legacy_functional(named, modifier_param, event_type, app_cursor)
    {
        return Some(bytes);
    }

    if let Some(cp) = kitty_functional_codepoint(named, location) {
        // Modifier/lock keys are reported only under report-all-keys or
        // event-type reporting; otherwise they are swallowed.
        if is_kitty_modifier_codepoint(cp)
            && !flags.report_all_keys()
            && !flags.report_event_types()
        {
            return None;
        }

        // Text-like specials (Esc/Enter/Tab/Backspace) emit CSI-u under
        // `disambiguate`; Space stays raw until modified/event-typed/all-keys.
        let always_csi = is_kitty_text_special(cp) && flags.disambiguate();
        let needs_csi = always_csi
            || flags.report_all_keys()
            || modifier_param.is_some()
            || event_type.is_some()
            || is_kitty_modifier_codepoint(cp);

        if needs_csi {
            return Some(build_csi_u_seq(cp, &[], modifier_param, event_type, text_codepoints));
        }
    }

    // No enhancement forces CSI-u for this key: fall back to the exact legacy
    // named-key encoding (press-only).
    if !pressed {
        return None;
    }
    translate_named_legacy(named, modifiers, app_cursor)
}

fn translate_named_kitty_legacy_functional(
    named: NamedKey,
    modifier_param: Option<u8>,
    event_type: Option<u8>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    translate_named_csi_letter_with_event(named, modifier_param, event_type, app_cursor)
        .or_else(|| translate_named_csi_tilde_with_event(named, modifier_param, event_type))
        .or_else(|| {
            translate_named_kitty_function_key_with_event(named, modifier_param, event_type)
        })
}

fn translate_named_kitty_function_key_with_event(
    named: NamedKey,
    modifier_param: Option<u8>,
    event_type: Option<u8>,
) -> Option<Vec<u8>> {
    kitty_csi_letter_for_fkey(named)
        .map(|letter| build_csi_letter_seq(letter, modifier_param, event_type))
        .or_else(|| {
            kitty_fkey_tilde_code(named)
                .map(|code| build_csi_tilde_seq(code, modifier_param, event_type))
        })
}

fn kitty_csi_letter_for_fkey(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::F1 => Some(b'P'),
        NamedKey::F2 => Some(b'Q'),
        NamedKey::F4 => Some(b'S'),
        _ => None,
    }
}

fn kitty_fkey_tilde_code(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::F3 => Some(13),
        NamedKey::F5 => Some(15),
        NamedKey::F6 => Some(17),
        NamedKey::F7 => Some(18),
        NamedKey::F8 => Some(19),
        NamedKey::F9 => Some(20),
        NamedKey::F10 => Some(21),
        NamedKey::F11 => Some(23),
        NamedKey::F12 => Some(24),
        _ => None,
    }
}

/// Resolve the unshifted base Unicode codepoint for a character key.
///
/// Prefers the explicit base character (winit's `key_without_modifiers`);
/// otherwise degrades to the lowercased logical character.
fn base_codepoint_for_character(input: &KeyInput, logical: char) -> u32 {
    if let Some(base) = input.base {
        return u32::from(base);
    }
    u32::from(logical.to_ascii_lowercase())
}

/// Build the `:shifted` alternate-codepoint list when `report_alternate_keys`
/// is negotiated and the shifted glyph differs from the base key.
fn alternate_codepoints(logical: char, base: u32, flags: KittyFlags) -> Vec<u32> {
    if !flags.report_alternate_keys() {
        return Vec::new();
    }
    let shifted = u32::from(logical);
    if shifted == 0 || shifted == base {
        return Vec::new();
    }
    vec![shifted]
}

/// Unicode scalar values of the event's associated text, when
/// `report_associated_text` is negotiated and the event carries text.
///
/// C0/C1 controls are excluded as required by the Kitty protocol; release
/// events carry no associated text.
fn associated_text_codepoints(input: &KeyInput, flags: KittyFlags) -> Vec<u32> {
    if !flags.report_associated_text() || input.state == KeyState::Released {
        return Vec::new();
    }
    input
        .text
        .as_ref()
        .map(|text| {
            text.chars()
                .map(u32::from)
                .filter(|codepoint| !matches!(codepoint, 0..=31 | 127..=159))
                .collect()
        })
        .unwrap_or_default()
}

/// Map a [`NamedKey`] to its Kitty CSI-u functional-key codepoint.
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

/// Modifier/lock key codepoints, disambiguated left/right by location.
fn kitty_modifier_lock_codepoint(named: NamedKey, location: KeyLocation) -> Option<u32> {
    let right_side = location == KeyLocation::Right;
    let cp = match named {
        NamedKey::Shift => {
            if right_side {
                57447
            } else {
                57441
            }
        }
        NamedKey::Control => {
            if right_side {
                57448
            } else {
                57442
            }
        }
        NamedKey::Alt => {
            if right_side {
                57449
            } else {
                57443
            }
        }
        NamedKey::Super => {
            if right_side {
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
/// alone (Esc, Enter, Tab, Backspace). Space (32) is excluded.
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

#[cfg(test)]
mod tests;
