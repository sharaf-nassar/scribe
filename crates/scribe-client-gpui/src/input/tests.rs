//! Golden byte-capture tests for the terminal input encoder.
//!
//! Each case reconstructs a [`KeyInput`] / [`TerminalMode`] pair and asserts the
//! encoder output is byte-identical to the fixture captured from the old
//! winit client (`tests/fixtures/gpui-client/keyboard-byte-golden.json`). The
//! fixture is the US1 correctness oracle: the GPUI port must not drift a single
//! byte from the legacy, Kitty, DECCKM, or DECPAM encodings.

use std::collections::BTreeMap;

use gpui::Modifiers;
use serde::Deserialize;

use super::{
    KeyInput, KeyLocation, KeyState, KeyToken, KittyFlags, NamedKey, TerminalMode, encode,
};

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<GoldenCase>,
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

fn load_fixture() -> BTreeMap<String, Vec<u8>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/gpui-client/keyboard-byte-golden.json"
    );
    let raw = std::fs::read_to_string(path).expect("read keyboard golden fixture");
    let fixture: Fixture = serde_json::from_str(&raw).expect("parse keyboard golden fixture");
    fixture.cases.into_iter().map(|c| (c.name, hex_to_bytes(&c.bytes))).collect()
}

/// Modifiers with no keys held.
fn no_mods() -> Modifiers {
    Modifiers::default()
}

/// Modifiers with just Control held.
fn ctrl() -> Modifiers {
    Modifiers { control: true, ..Modifiers::default() }
}

/// Modifiers with just Alt held.
fn alt() -> Modifiers {
    Modifiers { alt: true, ..Modifiers::default() }
}

/// Modifiers with just Shift held.
fn shift() -> Modifiers {
    Modifiers { shift: true, ..Modifiers::default() }
}

/// Modifiers with Control and Alt held.
fn ctrl_alt() -> Modifiers {
    Modifiers { control: true, alt: true, ..Modifiers::default() }
}

/// Build a [`KeyInput`] for a character key.
fn char_input(logical: char, base: char, modifiers: Modifiers) -> KeyInput {
    KeyInput {
        token: KeyToken::Char(logical),
        base: Some(base),
        text: Some(logical.to_string()),
        modifiers,
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
    }
}

/// Build a [`KeyInput`] for a named key.
fn named_input(named: NamedKey, modifiers: Modifiers) -> KeyInput {
    KeyInput {
        token: KeyToken::Named(named),
        base: None,
        text: None,
        modifiers,
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
    }
}

/// Reconstruct the encoder inputs for a fixture case by name.
fn build_case(name: &str) -> (KeyInput, TerminalMode) {
    let legacy = TerminalMode::legacy();
    let decckm = TerminalMode { app_cursor: true, ..TerminalMode::legacy() };
    let decpam = TerminalMode { app_keypad: true, ..TerminalMode::legacy() };
    let disambiguate =
        TerminalMode { kitty: KittyFlags::legacy_set().with_disambiguate(true), ..legacy };
    let all_keys =
        TerminalMode { kitty: KittyFlags::legacy_set().with_report_all_keys(true), ..legacy };
    let alternate =
        TerminalMode { kitty: KittyFlags::legacy_set().with_report_alternate_keys(true), ..legacy };
    let event_types =
        TerminalMode { kitty: KittyFlags::legacy_set().with_report_event_types(true), ..legacy };

    let numpad = |input: KeyInput| KeyInput { location: KeyLocation::Numpad, ..input };
    let repeat = |input: KeyInput| KeyInput { state: KeyState::Repeat, ..input };

    match name {
        "legacy-printable-a" => (char_input('a', 'a', no_mods()), legacy),
        "legacy-ctrl-c" => (char_input('c', 'c', ctrl()), legacy),
        "legacy-alt-c" => (char_input('c', 'c', alt()), legacy),
        "legacy-ctrl-alt-c" => (char_input('c', 'c', ctrl_alt()), legacy),
        "legacy-shift-tab" => (named_input(NamedKey::Tab, shift()), legacy),
        "legacy-ctrl-page-down" => (named_input(NamedKey::PageDown, ctrl()), legacy),
        "legacy-f1" => (named_input(NamedKey::F1, no_mods()), legacy),
        "legacy-arrow-up" => (named_input(NamedKey::ArrowUp, no_mods()), legacy),
        "legacy-shift-left" => (named_input(NamedKey::ArrowLeft, shift()), legacy),
        "decckm-arrow-up" => (named_input(NamedKey::ArrowUp, no_mods()), decckm),
        "decckm-home" => (named_input(NamedKey::Home, no_mods()), decckm),
        "decckm-ctrl-down" => (named_input(NamedKey::ArrowDown, ctrl()), decckm),
        "dec-pam-numpad-0" => (numpad(char_input('0', '0', no_mods())), decpam),
        "dec-pam-numpad-9" => (numpad(char_input('9', '9', no_mods())), decpam),
        "dec-pam-numpad-decimal" => (numpad(char_input('.', '.', no_mods())), decpam),
        "dec-pam-numpad-enter" => (numpad(named_input(NamedKey::Enter, no_mods())), decpam),
        "kitty-disambiguate-enter" => (named_input(NamedKey::Enter, no_mods()), disambiguate),
        "kitty-disambiguate-ctrl-i" => (char_input('i', 'i', ctrl()), disambiguate),
        "kitty-all-keys-a" => (char_input('a', 'a', no_mods()), all_keys),
        "kitty-alternate-shift-a" => (char_input('A', 'a', shift()), alternate),
        "kitty-repeat-arrow-left" => {
            (repeat(named_input(NamedKey::ArrowLeft, no_mods())), event_types)
        }
        "kitty-repeat-ctrl-arrow-left" => {
            (repeat(named_input(NamedKey::ArrowLeft, ctrl())), event_types)
        }
        "kitty-repeat-f13" => (repeat(named_input(NamedKey::F13, no_mods())), event_types),
        other => panic!("unmapped golden case: {other}"),
    }
}

// @lat: [[client#Input#GPUI Input Encoder Port]]
#[test]
fn keyboard_encoder_matches_golden_fixtures() {
    let golden = load_fixture();
    assert!(!golden.is_empty(), "fixture must contain cases");

    for (name, expected) in &golden {
        let (input, mode) = build_case(name);
        let actual =
            encode(&input, mode).unwrap_or_else(|| panic!("case {name} produced no bytes"));
        assert_eq!(actual, *expected, "case {name}: got {actual:02x?}, expected {expected:02x?}");
    }
}

#[test]
fn kitty_flags_round_trip_independently() {
    let flags = KittyFlags::legacy_set()
        .with_disambiguate(true)
        .with_report_event_types(true)
        .with_report_alternate_keys(true)
        .with_report_all_keys(true)
        .with_report_associated_text(true);
    assert!(flags.disambiguate());
    assert!(flags.report_event_types());
    assert!(flags.report_alternate_keys());
    assert!(flags.report_all_keys());
    assert!(flags.report_associated_text());
    assert!(flags.is_any());
    assert!(!flags.legacy());
    assert!(KittyFlags::legacy_set().legacy());
}

#[test]
fn released_key_without_event_reporting_emits_nothing() {
    let input =
        KeyInput { state: KeyState::Released, ..named_input(NamedKey::ArrowLeft, no_mods()) };
    // Legacy mode swallows releases.
    assert_eq!(encode(&input, TerminalMode::legacy()), None);
}
