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

use gpui::{Modifiers, MouseButton};
use serde::Deserialize;

use super::{
    MotionReporting, MouseReportMode, ScrollDirection, encode_mouse_motion, encode_mouse_press,
    encode_mouse_release, encode_mouse_scroll, should_report_mouse_motion,
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
