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
//! Alongside the encoders it owns the pure *decisions* the live path makes
//! around them: which DEC private modes are on ([`MouseModes`]), whether a
//! button event belongs to the application or to the client's own selection
//! gesture ([`MouseModes::forwards_buttons`]), how many terminal rows one wheel
//! event is worth ([`wheel_lines`]), and which of the three wheel consumers
//! claims it ([`wheel_action`]).
//!
//! All functions are pure — no side effects. Callers send the returned bytes
//! to the PTY via `ClientMessage::KeyInput`.

use gpui::{Modifiers, MouseButton, ScrollDelta};

/// The wire encoding an application has requested for mouse reports.
///
/// X10 is the default because it is what a terminal reports until the
/// application selects SGR with DECSET 1006.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum MouseReportMode {
    /// SGR 1006 (`\x1b[<Cb;Cx;Cy{M,m}`), the modern encoding.
    Sgr,
    /// Legacy X10 (`\x1b[M<byte><byte><byte>`).
    #[default]
    X10,
}

/// The mouse-related DEC private modes a pane's application currently has on.
///
/// Read off the pane's `Term` once per event and then passed by value, so every
/// decision below stays a pure function of terminal state rather than reaching
/// back into the grid mid-gesture.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MouseModes {
    /// The motion level 1000 / 1002 / 1003 selected, or `None` when the
    /// application asked for no mouse tracking at all. The two are one field
    /// because a motion level only means anything while tracking is on.
    pub tracking: Option<MotionReporting>,
    /// The wire encoding reports must use; 1006 selects SGR.
    pub encoding: MouseReportMode,
    /// The alternate screen buffer is showing (1049 / 47).
    pub alt_screen: bool,
    /// 1007: on the alternate screen, a wheel tick becomes cursor keys.
    pub alternate_scroll: bool,
}

impl MouseModes {
    /// How much pointer motion the application asked to hear about, which is
    /// none at all when it tracks the mouse not at all.
    #[must_use]
    pub const fn motion(self) -> MotionReporting {
        match self.tracking {
            Some(motion) => motion,
            None => MotionReporting::None,
        }
    }

    /// Whether a button press / release / motion belongs to the application
    /// rather than to the client's own selection gesture.
    ///
    /// Shift is the universal escape hatch every terminal offers: holding it
    /// takes the pointer back from a mouse-tracking application so the user can
    /// still select text inside vim or tmux.
    #[must_use]
    pub const fn forwards_buttons(self, shift_held: bool) -> bool {
        self.tracking.is_some() && !shift_held
    }
}

/// The direction of a scroll-wheel event.
#[derive(Clone, Copy)]
pub enum ScrollDirection {
    Up,
    Down,
}

impl ScrollDirection {
    /// The direction a signed row delta from [`wheel_lines`] scrolls in.
    ///
    /// Positive rows walk backwards into the scrollback, which is what button
    /// 64 (`Up`) means to an application.
    #[must_use]
    pub const fn from_rows(rows: i32) -> Self {
        if rows > 0 { Self::Up } else { Self::Down }
    }
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
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum MotionReporting {
    /// Mode 1000 (click only): no motion is reported.
    #[default]
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

/// Who claims one wheel event, in the priority order xterm defines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WheelAction {
    /// The application tracks the mouse: encode button 64 / 65 and send it.
    Report,
    /// Alternate screen plus alternate scroll (1007): send cursor keys, so a
    /// pager that never asked for mouse tracking still scrolls on the wheel.
    CursorKeys,
    /// Nothing claimed the tick, so it moves the client's own viewport through
    /// the local scrollback.
    Scrollback,
}

/// Decide what one wheel event does, given the pane's live terminal modes.
///
/// Mouse tracking wins outright — an application that asked for reports gets
/// them even on the normal screen. Alternate scroll is the fallback for the
/// alternate screen only, exactly as the winit client ordered it.
#[must_use]
pub const fn wheel_action(modes: MouseModes) -> WheelAction {
    if modes.tracking.is_some() {
        WheelAction::Report
    } else if modes.alt_screen && modes.alternate_scroll {
        WheelAction::CursorKeys
    } else {
        WheelAction::Scrollback
    }
}

/// How many terminal rows one wheel event is worth, signed so that a positive
/// value walks backwards into the scrollback.
///
/// GPUI reports a notched wheel as [`ScrollDelta::Lines`] already scaled by its
/// three-rows-per-notch constant — the same factor the winit client multiplied
/// in by hand — so the line form needs no scaling of its own. A trackpad's
/// [`ScrollDelta::Pixels`] is divided by the row height instead.
///
/// The platform sign is "positive `y` reveals the content above", which is
/// traditional terminal behaviour, so `natural_scroll` (off by default, per
/// `terminal.scroll.natural_scroll`) is the branch that inverts.
#[must_use]
pub fn wheel_lines(delta: ScrollDelta, line_height: f32, natural_scroll: bool) -> i32 {
    let rows = match delta {
        ScrollDelta::Lines(lines) => round_to_i32(lines.y),
        ScrollDelta::Pixels(pixels) => {
            if !line_height.is_finite() || line_height <= 0.0 {
                return 0;
            }
            round_to_i32(f32::from(pixels.y) / line_height)
        }
    };
    if natural_scroll { -rows } else { rows }
}

/// The cursor-key burst mode 1007 wants in place of a wheel report: one CUU
/// (`\x1b[A`) per row backwards, one CUD (`\x1b[B`) per row forwards.
#[must_use]
pub fn alternate_scroll_keys(rows: i32) -> Vec<u8> {
    let sequence: &[u8] = if rows > 0 { b"\x1b[A" } else { b"\x1b[B" };
    let count = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
    sequence.iter().copied().cycle().take(sequence.len().saturating_mul(count)).collect()
}

/// Round a signed row count to `i32` without a float-to-int cast, which the
/// workspace lints deny.
///
/// Sub-row travel rounds to zero rather than to a phantom row, which is what
/// makes a trackpad's fine-grained pixel deltas usable at all.
fn round_to_i32(value: f32) -> i32 {
    let magnitude = i32::from(magnitude_units(value.abs().round()));
    if value < 0.0 { -magnitude } else { magnitude }
}

/// The largest `u16` not exceeding `magnitude`, resolved by binary search so no
/// float-to-int cast is needed. `magnitude` is already integer-valued at every
/// call site, so this reproduces it exactly (clamped to `u16::MAX`, far more
/// rows than any single wheel event can carry).
fn magnitude_units(magnitude: f32) -> u16 {
    if !magnitude.is_finite() || magnitude < 1.0 {
        return 0;
    }
    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if f32::from(mid) <= magnitude {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    low
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
