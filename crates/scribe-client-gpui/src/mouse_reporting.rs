//! xterm mouse protocol encoding (SGR mode 1006 and X10) for the GPUI client.
//!
//! This is a byte-for-byte port of the winit-driven reporter in
//! `crates/scribe-client/src/mouse_reporting.rs`, retargeted at GPUI's
//! [`gpui::MouseButton`] / [`gpui::Modifiers`] model. It reproduces the X10 and
//! SGR-1006 encodings, the modifier bits packed into the Cb byte, scroll-wheel
//! reporting, held-button motion, and the mode 1000/1002/1003 motion gate with
//! per-cell de-duplication. The golden fixtures in
//! `tests/fixtures/gpui-client/mouse-byte-golden.json` are the correctness
//! oracle (US1): every encoder path must stay byte-identical to the old client.
//!
//! All functions are pure — no side effects. Callers send the returned bytes
//! to the PTY via `ClientMessage::KeyInput`.

use gpui::{Modifiers, MouseButton};

/// The wire encoding an application has requested for mouse reports.
#[derive(Clone, Copy)]
pub enum MouseReportMode {
    /// SGR 1006 (`\x1b[<Cb;Cx;Cy{M,m}`), the default modern encoding.
    Sgr,
    /// Legacy X10 (`\x1b[M<byte><byte><byte>`).
    X10,
}

/// The direction of a scroll-wheel event.
#[derive(Clone, Copy)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Encode modifier bits into the Cb byte per xterm spec.
///
/// +4 = Shift, +8 = Alt, +16 = Ctrl.
fn modifier_bits(modifiers: Modifiers) -> u8 {
    let mut bits: u8 = 0;
    if modifiers.shift {
        bits |= 4;
    }
    if modifiers.alt {
        bits |= 8;
    }
    if modifiers.control {
        bits |= 16;
    }
    bits
}

/// Map a [`MouseButton`] to its xterm Cb base value.
///
/// Returns `None` for buttons that have no xterm encoding.
fn button_base(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Navigate(_) => None,
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
    modifiers: Modifiers,
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
    modifiers: Modifiers,
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
    modifiers: Modifiers,
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
    modifiers: Modifiers,
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
mod tests;
