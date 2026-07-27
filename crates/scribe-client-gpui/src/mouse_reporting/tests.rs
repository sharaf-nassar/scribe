//! Golden byte-capture tests for the terminal mouse reporter.
//!
//! Each `cases` entry reconstructs an encoder call and asserts the output is
//! byte-identical to the fixture captured from the old winit client
//! (`tests/fixtures/gpui-client/mouse-byte-golden.json`). The `motion-gates`
//! entries replay [`should_report_mouse_motion`] against the captured
//! 1000/1002/1003 truth table. The fixture is the US1 correctness oracle: the
//! GPUI port must not drift a single byte from the legacy X10 / SGR-1006
//! encodings or the motion gate.

use std::collections::BTreeMap;

use gpui::{Modifiers, MouseButton, ScrollDelta, point, px};
use serde::Deserialize;

use super::{
    MotionReporting, MouseModes, MouseReportMode, ScrollDirection, WheelAction,
    alternate_scroll_keys, encode_mouse_motion, encode_mouse_press, encode_mouse_release,
    encode_mouse_scroll, should_report_mouse_motion, wheel_action, wheel_lines,
};

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<GoldenCase>,
    // Each gate row carries three booleans (button_held / cell_changed /
    // report); a typed struct would trip `clippy::struct_excessive_bools`, so
    // rows are kept as raw JSON and their fields pulled out in the test.
    #[serde(rename = "motion-gates")]
    motion_gates: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct GoldenCase {
    name: String,
    bytes: String,
}

/// Decode a lowercase-hex string into bytes.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex pair"))
        .collect()
}

fn load_fixture() -> Fixture {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/gpui-client/mouse-byte-golden.json"
    );
    let raw = std::fs::read_to_string(path).expect("read mouse golden fixture");
    serde_json::from_str(&raw).expect("parse mouse golden fixture")
}

/// Modifiers with no keys held.
fn no_mods() -> Modifiers {
    Modifiers::default()
}

/// Modifiers with just Shift held.
fn shift() -> Modifiers {
    Modifiers { shift: true, ..Modifiers::default() }
}

/// Modifiers with just Alt held.
fn alt() -> Modifiers {
    Modifiers { alt: true, ..Modifiers::default() }
}

/// Modifiers with just Control held.
fn ctrl() -> Modifiers {
    Modifiers { control: true, ..Modifiers::default() }
}

/// Reconstruct the encoder output for a fixture case by name.
fn encode_case(name: &str) -> Vec<u8> {
    use MouseReportMode::{Sgr, X10};
    match name {
        "sgr-1006-left-press" => encode_mouse_press(MouseButton::Left, 0, 0, no_mods(), Sgr),
        "sgr-1006-right-release-shift" => {
            encode_mouse_release(MouseButton::Right, 3, 5, shift(), Sgr)
        }
        "sgr-1006-scroll-down-ctrl" => {
            encode_mouse_scroll(ScrollDirection::Down, 11, 7, ctrl(), Sgr)
        }
        "sgr-1006-drag-right-alt" => {
            encode_mouse_motion(4, 7, Some(MouseButton::Right), alt(), Sgr)
        }
        "x10-left-press" => encode_mouse_press(MouseButton::Left, 0, 0, no_mods(), X10),
        "x10-release" => encode_mouse_release(MouseButton::Right, 4, 6, no_mods(), X10),
        "x10-middle-drag" => encode_mouse_motion(0, 0, Some(MouseButton::Middle), no_mods(), X10),
        "x10-coordinate-clamp" => encode_mouse_press(MouseButton::Left, 999, 999, no_mods(), X10),
        other => panic!("unmapped golden case: {other}"),
    }
}

/// Map a fixture `mode` string (`"1000"`/`"1002"`/`"1003"`) to its
/// [`MotionReporting`] level.
fn motion_reporting(mode: &str) -> MotionReporting {
    match mode {
        "1000" => MotionReporting::None,
        "1002" => MotionReporting::Drag,
        "1003" => MotionReporting::Any,
        other => panic!("unmapped motion mode: {other}"),
    }
}

// @lat: [[client#Input#Mouse Reporting#GPUI Rebuild Golden Oracle]]
#[test]
fn mouse_encoder_matches_golden_fixtures() {
    let fixture = load_fixture();
    assert!(!fixture.cases.is_empty(), "fixture must contain cases");

    let golden: BTreeMap<String, Vec<u8>> =
        fixture.cases.into_iter().map(|c| (c.name, hex_to_bytes(&c.bytes))).collect();

    for (name, expected) in &golden {
        let actual = encode_case(name);
        assert_eq!(actual, *expected, "case {name}: got {actual:02x?}, expected {expected:02x?}");
    }
}

#[test]
fn mouse_motion_gate_matches_golden_fixtures() {
    let fixture = load_fixture();
    assert!(!fixture.motion_gates.is_empty(), "fixture must contain motion gates");

    // The pointer is always at a fixed cell; `cell_changed` selects whether the
    // last reported cell differs from it, matching the fixture's per-mode gate.
    let cell = (1, 1);
    for gate in &fixture.motion_gates {
        let mode = gate["mode"].as_str().expect("gate mode string");
        let button_held = gate["button_held"].as_bool().expect("gate button_held bool");
        let cell_changed = gate["cell_changed"].as_bool().expect("gate cell_changed bool");
        let expected = gate["report"].as_bool().expect("gate report bool");

        let last_reported = if cell_changed { Some((0, 0)) } else { Some(cell) };
        let reported =
            should_report_mouse_motion(motion_reporting(mode), button_held, cell, last_reported);
        assert_eq!(
            reported, expected,
            "mode {mode} held={button_held} cell_changed={cell_changed}: expected report={expected}"
        );
    }
}

/// Modes with mouse tracking on at the given motion level and SGR encoding.
fn tracking(motion: MotionReporting) -> MouseModes {
    MouseModes { tracking: Some(motion), encoding: MouseReportMode::Sgr, ..MouseModes::default() }
}

// @lat: [[client#Input#Mouse Reporting#Live-Path Decisions]]
#[test]
fn wheel_action_orders_its_three_consumers() {
    // Mouse tracking wins outright, on either screen buffer.
    assert_eq!(wheel_action(tracking(MotionReporting::None)), WheelAction::Report);
    let alt_tracking =
        MouseModes { alt_screen: true, alternate_scroll: true, ..tracking(MotionReporting::Any) };
    assert_eq!(wheel_action(alt_tracking), WheelAction::Report);

    // Alternate scroll is the alternate screen's fallback, and only there.
    let alt_scroll =
        MouseModes { alt_screen: true, alternate_scroll: true, ..MouseModes::default() };
    assert_eq!(wheel_action(alt_scroll), WheelAction::CursorKeys);
    assert_eq!(
        wheel_action(MouseModes { alt_screen: false, ..alt_scroll }),
        WheelAction::Scrollback
    );
    assert_eq!(
        wheel_action(MouseModes { alternate_scroll: false, ..alt_scroll }),
        WheelAction::Scrollback
    );

    // Nothing enabled at all is the ordinary shell prompt.
    assert_eq!(wheel_action(MouseModes::default()), WheelAction::Scrollback);
}

#[test]
fn wheel_lines_reads_both_delta_forms_and_honours_natural_scroll() {
    // GPUI already scales a notch to three rows, so the line form passes
    // through. Traditional (the default) keeps the platform sign, where a
    // wheel-up notch is positive and walks into the scrollback.
    let notch_up = ScrollDelta::Lines(point(0.0, 3.0));
    let notch_down = ScrollDelta::Lines(point(0.0, -3.0));
    assert_eq!(wheel_lines(notch_up, 18.0, false), 3);
    assert_eq!(wheel_lines(notch_down, 18.0, false), -3);
    assert_eq!(wheel_lines(notch_up, 18.0, true), -3);

    // A trackpad's pixel delta is divided by the row height and rounded.
    assert_eq!(wheel_lines(ScrollDelta::Pixels(point(px(0.0), px(36.0))), 18.0, false), 2);
    assert_eq!(wheel_lines(ScrollDelta::Pixels(point(px(0.0), px(-45.0))), 18.0, false), -3);
    // Sub-row travel rounds to nothing rather than to a phantom row.
    assert_eq!(wheel_lines(ScrollDelta::Pixels(point(px(0.0), px(4.0))), 18.0, false), 0);
    // A degenerate row height cannot divide, so the event is dropped.
    assert_eq!(wheel_lines(ScrollDelta::Pixels(point(px(0.0), px(36.0))), 0.0, false), 0);
}

#[test]
fn alternate_scroll_sends_one_cursor_key_per_row() {
    assert_eq!(alternate_scroll_keys(2), b"\x1b[A\x1b[A".to_vec());
    assert_eq!(alternate_scroll_keys(-3), b"\x1b[B\x1b[B\x1b[B".to_vec());
    assert_eq!(alternate_scroll_keys(0), Vec::<u8>::new());
}

#[test]
fn shift_takes_the_pointer_back_from_a_tracking_application() {
    let modes = tracking(MotionReporting::Any);
    assert!(modes.forwards_buttons(false));
    assert!(!modes.forwards_buttons(true));
    // With no tracking at all the pointer is always the client's.
    assert!(!MouseModes::default().forwards_buttons(false));
}

#[test]
fn scroll_direction_follows_the_signed_row_delta() {
    assert!(matches!(ScrollDirection::from_rows(3), ScrollDirection::Up));
    assert!(matches!(ScrollDirection::from_rows(-3), ScrollDirection::Down));
    // The encoding of each direction is the golden fixture's, so a wired wheel
    // is byte-identical to the winit client's.
    let up =
        encode_mouse_scroll(ScrollDirection::from_rows(1), 0, 0, no_mods(), MouseReportMode::Sgr);
    assert_eq!(up, b"\x1b[<64;1;1M".to_vec());
    let down =
        encode_mouse_scroll(ScrollDirection::from_rows(-1), 0, 0, no_mods(), MouseReportMode::Sgr);
    assert_eq!(down, b"\x1b[<65;1;1M".to_vec());
}
