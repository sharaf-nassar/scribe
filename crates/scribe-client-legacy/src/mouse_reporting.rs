//! xterm mouse protocol encoding (SGR mode 1006 and X10).
//!
//! All functions are pure — no side effects. Callers send the returned bytes
//! to the PTY via `ClientCommand::KeyInput`.

use winit::event::MouseButton;
use winit::keyboard::ModifiersState;

#[derive(Clone, Copy)]
pub enum MouseReportMode {
    Sgr,
    X10,
}

#[derive(Clone, Copy)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Encode modifier bits into the Cb byte per xterm spec.
///
/// +4 = Shift, +8 = Alt, +16 = Ctrl.
fn modifier_bits(modifiers: ModifiersState) -> u8 {
    let mut bits: u8 = 0;
    if modifiers.shift_key() {
        bits |= 4;
    }
    if modifiers.alt_key() {
        bits |= 8;
    }
    if modifiers.control_key() {
        bits |= 16;
    }
    bits
}

/// Map a `MouseButton` to its xterm Cb base value.
///
/// Returns `None` for buttons that have no xterm encoding.
fn button_base(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

/// Encode a mouse button press event.
///
/// Returns the escape sequence bytes to send to the PTY. Returns an empty
/// `Vec` if the button has no xterm encoding.
pub fn encode_mouse_press(
    button: MouseButton,
    col: u16,
    row: u16,
    modifiers: ModifiersState,
    mode: MouseReportMode,
) -> Vec<u8> {
    let Some(base) = button_base(button) else { return Vec::new() };
    let cb = base | modifier_bits(modifiers);
    encode_button_report(mode, cb, col, row, true)
}

/// Encode a mouse button release event.
///
/// In SGR mode the exact button is preserved. In X10 mode, release is
/// encoded as button 3 (no button information available).
pub fn encode_mouse_release(
    button: MouseButton,
    col: u16,
    row: u16,
    modifiers: ModifiersState,
    mode: MouseReportMode,
) -> Vec<u8> {
    let Some(base) = button_base(button) else { return Vec::new() };
    let cb = base | modifier_bits(modifiers);
    match mode {
        MouseReportMode::Sgr => encode_sgr(cb, col, row, false),
        MouseReportMode::X10 => {
            // X10 release: Cb = 3, modifiers not preserved.
            encode_x10(3, col, row)
        }
    }
}

/// Encode a scroll wheel event.
///
/// Button 64 = scroll up, 65 = scroll down per xterm spec.
pub fn encode_mouse_scroll(
    direction: ScrollDirection,
    col: u16,
    row: u16,
    modifiers: ModifiersState,
    mode: MouseReportMode,
) -> Vec<u8> {
    let base: u8 = match direction {
        ScrollDirection::Up => 64,
        ScrollDirection::Down => 65,
    };
    let cb = base | modifier_bits(modifiers);
    encode_button_report(mode, cb, col, row, true)
}

/// Encode a mouse motion event.
///
/// The motion flag (+32) is added to the Cb value. When a button is held,
/// its base value is `OR`ed in; otherwise the base is 0 (no button).
pub fn encode_mouse_motion(
    col: u16,
    row: u16,
    button_held: Option<MouseButton>,
    modifiers: ModifiersState,
    mode: MouseReportMode,
) -> Vec<u8> {
    let base = button_held.and_then(button_base).unwrap_or(0);
    let cb = base | 32 | modifier_bits(modifiers);
    encode_button_report(mode, cb, col, row, true)
}

/// The pointer-motion reporting level an application has requested.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MotionReporting {
    /// Mode 1000 (click only): no motion is reported.
    None,
    /// Mode 1002 (button-event): motion is reported only while a button is held.
    Drag,
    /// Mode 1003 (any-event): all motion is reported.
    Any,
}

/// Decide whether a pointer-motion event should be reported to the application.
///
/// Mirrors xterm / alacritty motion semantics: mode 1003 (`Any`) reports all
/// motion; mode 1002 (`Drag`) reports motion only while a button is held; mode
/// 1000 (`None`) reports nothing. A report is also suppressed unless the
/// pointer moved to a different cell than the last reported one, per xterm's
/// "reported only if the pointer has moved to a different character cell".
#[must_use]
pub fn should_report_mouse_motion(
    reporting: MotionReporting,
    button_held: bool,
    cell: (u16, u16),
    last_reported: Option<(u16, u16)>,
) -> bool {
    let enabled = match reporting {
        MotionReporting::Any => true,
        MotionReporting::Drag => button_held,
        MotionReporting::None => false,
    };
    enabled && last_reported != Some(cell)
}

fn encode_button_report(mode: MouseReportMode, cb: u8, col: u16, row: u16, press: bool) -> Vec<u8> {
    match mode {
        MouseReportMode::Sgr => encode_sgr(cb, col, row, press),
        MouseReportMode::X10 => encode_x10(cb, col, row),
    }
}

/// Build an SGR (`\x1b[<Cb;Cx;CyM` or `\x1b[<Cb;Cx;Cym`) sequence.
///
/// `press` is `true` for press/motion, `false` for release.
/// `col` and `row` are 0-indexed viewport coordinates; the sequence uses
/// 1-indexed values as required by the protocol.
fn encode_sgr(cb: u8, col: u16, row: u16, press: bool) -> Vec<u8> {
    let trailer = if press { b'M' } else { b'm' };
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);
    format!("\x1b[<{cb};{cx};{cy}{}", trailer as char).into_bytes()
}

/// Build an X10 (`\x1b[M<byte><byte><byte>`) sequence.
///
/// `col` and `row` are 0-indexed; the protocol uses 1-indexed values offset
/// by 32. Coordinates are clamped to 222 (max 0-indexed value encodable in
/// a single byte: 222 + 1 + 32 = 255).
fn encode_x10(cb: u8, col: u16, row: u16) -> Vec<u8> {
    let cx = col.min(222).saturating_add(1) as u8;
    let cy = row.min(222).saturating_add(1) as u8;
    vec![b'\x1b', b'[', b'M', cb.saturating_add(32), cx.saturating_add(32), cy.saturating_add(32)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Motion encoding must combine the button base with the +32 motion bit.
    /// Left=0 → 32, Middle=1 → 33, Right=2 → 34 (no modifiers).
    #[test]
    fn sgr_motion_includes_button_base_plus_motion_bit() {
        let mods = ModifiersState::empty();

        let left = encode_mouse_motion(0, 0, Some(MouseButton::Left), mods, MouseReportMode::Sgr);
        assert_eq!(left, b"\x1b[<32;1;1M".to_vec());

        let middle =
            encode_mouse_motion(5, 9, Some(MouseButton::Middle), mods, MouseReportMode::Sgr);
        // Cb = 1 (Middle) | 32 = 33; col/row are 1-indexed in the sequence.
        assert_eq!(middle, b"\x1b[<33;6;10M".to_vec());

        let right = encode_mouse_motion(5, 9, Some(MouseButton::Right), mods, MouseReportMode::Sgr);
        // Cb = 2 (Right) | 32 = 34.
        assert_eq!(right, b"\x1b[<34;6;10M".to_vec());
    }

    /// In X10 mode the Cb byte (button base + 32 motion) is offset by a further
    /// +32 in the wire encoding: Middle → 33+32 = 65 ('A'), Right → 34+32 = 66.
    #[test]
    fn x10_motion_includes_button_base_plus_motion_bit() {
        let mods = ModifiersState::empty();

        let middle =
            encode_mouse_motion(0, 0, Some(MouseButton::Middle), mods, MouseReportMode::X10);
        // Cb = 33; wire byte = 33 + 32 = 65; coords = (0+1)+32 = 33.
        assert_eq!(middle, vec![b'\x1b', b'[', b'M', 65, 33, 33]);

        let right = encode_mouse_motion(0, 0, Some(MouseButton::Right), mods, MouseReportMode::X10);
        // Cb = 34; wire byte = 34 + 32 = 66.
        assert_eq!(right, vec![b'\x1b', b'[', b'M', 66, 33, 33]);
    }

    /// Motion with no button held in mode 1003 encodes the bare motion bit:
    /// base 0 | 32 = 32, with no modifiers. Covers the mode-1003 no-button
    /// path where `encode_mouse_motion` receives `None`.
    #[test]
    fn sgr_motion_without_button_uses_motion_bit_only() {
        let motion = encode_mouse_motion(4, 7, None, ModifiersState::empty(), MouseReportMode::Sgr);
        // Cb = 0 (no button) | 32 (motion) = 32; col/row are 1-indexed: 5,8.
        assert_eq!(motion, b"\x1b[<32;5;8M".to_vec());
    }

    // ── should_report_mouse_motion gating semantics ─────────────────
    //
    // These mirror xterm / alacritty per-mode motion reporting:
    //   mode 1000 -> any_motion=false, drag=false  (click only, no motion)
    //   mode 1002 -> drag=true                      (motion only while held)
    //   mode 1003 -> any_motion=true                (all motion)
    // plus xterm's "different character cell" de-duplication guard.

    /// Mode 1000 (click-only): no motion is ever reported, whether or not a
    /// button is held.
    #[test]
    fn mode_1000_suppresses_motion() {
        // Click-only mode never reports motion, with or without a button.
        assert!(!should_report_mouse_motion(MotionReporting::None, false, (3, 4), None));
        assert!(!should_report_mouse_motion(MotionReporting::None, true, (3, 4), None));
    }

    /// Mode 1002 (button-event / drag): motion is reported only while a button
    /// is held.
    #[test]
    fn mode_1002_requires_held_button() {
        // Drag mode but no button held -> suppressed.
        assert!(!should_report_mouse_motion(MotionReporting::Drag, false, (3, 4), None));
        // Drag mode with a button held, fresh cell -> reported.
        assert!(should_report_mouse_motion(MotionReporting::Drag, true, (3, 4), None));
    }

    /// Mode 1003 (any-motion): motion is reported regardless of button state.
    #[test]
    fn mode_1003_reports_without_button() {
        // Any-motion is reported regardless of button state.
        assert!(should_report_mouse_motion(MotionReporting::Any, false, (3, 4), None));
        assert!(should_report_mouse_motion(MotionReporting::Any, true, (3, 4), None));
    }

    /// xterm de-duplication: a motion event staying within the previously
    /// reported cell is suppressed; moving to a different cell is reported.
    #[test]
    fn motion_deduplicated_within_same_cell() {
        // Same cell as last report -> suppressed.
        assert!(!should_report_mouse_motion(MotionReporting::Any, false, (5, 5), Some((5, 5))));
        // Different cell -> reported.
        assert!(should_report_mouse_motion(MotionReporting::Any, false, (6, 5), Some((5, 5))));
    }
}
