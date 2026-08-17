//! Keybinding parser and dispatch tests.
//!
//! Verifies the full port: combo-string parsing, exact-modifier matching, the
//! three-level dispatch order, and — crucially for parity — that every one of
//! the 50+ configurable actions resolves to its named [`LayoutAction`] /
//! [`KeyAction`] from the default keybindings.

use gpui::Modifiers;
use scribe_common::config::KeybindingsConfig;

use super::{
    Bindings, KeyAction, Keybinding, LayoutAction, OVERLAY_CHORDS, translate_key_action,
    translate_layout_shortcut, translate_overlay_chord, translate_terminal_shortcut,
};
use crate::input::{KeyInput, KeyLocation, KeyState, KeyToken, NamedKey};

/// Build a `KeyInput` that exactly satisfies `binding`, in the given state.
fn input_for(binding: &Keybinding, state: KeyState) -> KeyInput {
    let (token, base) = match binding.key {
        super::KeyMatch::Character(c) => (KeyToken::Char(c), Some(c)),
        super::KeyMatch::Named(n) => (KeyToken::Named(n), None),
    };
    KeyInput {
        token,
        base,
        text: None,
        modifiers: binding.modifiers,
        location: KeyLocation::Standard,
        state,
    }
}

/// Build a pressed `KeyInput` from the first binding in a set.
fn pressed_first(set: &[Keybinding]) -> KeyInput {
    input_for(set.first().expect("action has a default binding"), KeyState::Pressed)
}

#[test]
fn parse_reads_modifiers_and_char() {
    let kb = Keybinding::parse("ctrl+shift+w").expect("valid");
    assert!(kb.modifiers.control);
    assert!(kb.modifiers.shift);
    assert!(!kb.modifiers.alt);
    assert!(!kb.modifiers.platform);
    assert_eq!(kb.key, super::KeyMatch::Character('w'));
}

#[test]
fn parse_maps_cmd_and_super_to_platform() {
    for combo in ["cmd+t", "super+t"] {
        let kb = Keybinding::parse(combo).expect("valid");
        assert!(kb.modifiers.platform, "{combo} should set platform");
        assert_eq!(kb.key, super::KeyMatch::Character('t'));
    }
}

#[test]
fn parse_recognises_named_keys() {
    let cases = [
        ("tab", NamedKey::Tab),
        ("enter", NamedKey::Enter),
        ("return", NamedKey::Enter),
        ("esc", NamedKey::Escape),
        ("pageup", NamedKey::PageUp),
        ("left", NamedKey::ArrowLeft),
    ];
    for (combo, named) in cases {
        assert_eq!(Keybinding::parse(combo).expect("valid").key, super::KeyMatch::Named(named));
    }
}

#[test]
fn parse_rejects_unknown_key() {
    assert!(Keybinding::parse("ctrl+nonsense").is_none());
    assert!(Keybinding::parse("ctrl+shift").is_none());
}

#[test]
fn matches_requires_exact_modifiers() {
    let kb = Keybinding::parse("ctrl+w").expect("valid");
    let mut extra = input_for(&kb, KeyState::Pressed);
    extra.modifiers.shift = true;
    assert!(!kb.matches(&extra), "extra shift must not match ctrl+w");

    assert!(kb.matches(&input_for(&kb, KeyState::Pressed)));
}

#[test]
fn matches_ignores_gpui_function_flag() {
    let kb = Keybinding::parse("ctrl+w").expect("valid");
    let mut with_fn = input_for(&kb, KeyState::Pressed);
    with_fn.modifiers.function = true;
    assert!(kb.matches(&with_fn), "function flag must not block a match");
}

#[test]
fn matches_is_case_insensitive_on_base() {
    let kb = Keybinding::parse("ctrl+w").expect("valid");
    let mut upper = input_for(&kb, KeyState::Pressed);
    upper.base = Some('W');
    upper.token = KeyToken::Char('W');
    assert!(kb.matches(&upper));
}

#[test]
fn matches_only_on_key_down() {
    let kb = Keybinding::parse("ctrl+w").expect("valid");
    assert!(kb.matches(&input_for(&kb, KeyState::Pressed)));
    assert!(kb.matches(&input_for(&kb, KeyState::Repeat)));
    assert!(!kb.matches(&input_for(&kb, KeyState::Released)));
}

#[test]
fn shifted_symbol_matches_when_gpui_keeps_shift_set() {
    let kb = Keybinding::parse("cmd+shift+]").expect("valid");
    let input = KeyInput {
        token: KeyToken::Char('}'),
        base: Some('}'),
        text: Some("}".into()),
        modifiers: Modifiers { platform: true, shift: true, ..Modifiers::default() },
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
    };
    assert!(kb.matches(&input));
}

/// The whole point of the port: every named action must still resolve. Drives
/// each action from its default binding and asserts the exact intercepted
/// action — 43 layout actions plus palette/settings/find plus 7 terminal
/// shortcuts, covering the 50+ actions named in the parity inventory.
// @lat: [[test#GPUI Client Headless Suites#GPUI keybindings dispatch]]
#[test]
fn all_layout_actions_resolve_from_defaults() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    let layout_cases: &[(&[Keybinding], LayoutAction)] = &[
        (&bindings.split_vertical, LayoutAction::SplitVertical),
        (&bindings.split_horizontal, LayoutAction::SplitHorizontal),
        (&bindings.close_pane, LayoutAction::ClosePane),
        (&bindings.cycle_pane, LayoutAction::FocusNext),
        (&bindings.focus_left, LayoutAction::FocusLeft),
        (&bindings.focus_right, LayoutAction::FocusRight),
        (&bindings.focus_up, LayoutAction::FocusUp),
        (&bindings.focus_down, LayoutAction::FocusDown),
        (&bindings.equalize, LayoutAction::Equalize),
        (&bindings.workspace_split_vertical, LayoutAction::WorkspaceSplitVertical),
        (&bindings.workspace_split_horizontal, LayoutAction::WorkspaceSplitHorizontal),
        (&bindings.workspace_focus_left, LayoutAction::WorkspaceFocusLeft),
        (&bindings.workspace_focus_right, LayoutAction::WorkspaceFocusRight),
        (&bindings.workspace_focus_up, LayoutAction::WorkspaceFocusUp),
        (&bindings.workspace_focus_down, LayoutAction::WorkspaceFocusDown),
        (&bindings.new_tab, LayoutAction::NewTab),
        (&bindings.new_claude_tab, LayoutAction::NewClaudeTab),
        (&bindings.new_claude_resume_tab, LayoutAction::NewClaudeResumeTab),
        (&bindings.new_codex_tab, LayoutAction::NewCodexTab),
        (&bindings.new_codex_resume_tab, LayoutAction::NewCodexResumeTab),
        (&bindings.new_pi_tab, LayoutAction::NewPiTab),
        (&bindings.close_tab, LayoutAction::CloseTab),
        (&bindings.next_tab, LayoutAction::NextTab),
        (&bindings.prev_tab, LayoutAction::PrevTab),
        (&bindings.select_tab_1, LayoutAction::SelectTab(0)),
        (&bindings.select_tab_2, LayoutAction::SelectTab(1)),
        (&bindings.select_tab_3, LayoutAction::SelectTab(2)),
        (&bindings.select_tab_4, LayoutAction::SelectTab(3)),
        (&bindings.select_tab_5, LayoutAction::SelectTab(4)),
        (&bindings.select_tab_6, LayoutAction::SelectTab(5)),
        (&bindings.select_tab_7, LayoutAction::SelectTab(6)),
        (&bindings.select_tab_8, LayoutAction::SelectTab(7)),
        (&bindings.select_tab_9, LayoutAction::SelectTab(8)),
        (&bindings.new_window, LayoutAction::NewWindow),
        (&bindings.copy, LayoutAction::CopySelection),
        (&bindings.paste, LayoutAction::PasteClipboard),
        (&bindings.scroll_up, LayoutAction::ScrollUp),
        (&bindings.scroll_down, LayoutAction::ScrollDown),
        (&bindings.scroll_top, LayoutAction::ScrollTop),
        (&bindings.scroll_bottom, LayoutAction::ScrollBottom),
        (&bindings.prompt_jump_up, LayoutAction::PromptJumpUp),
        (&bindings.prompt_jump_down, LayoutAction::PromptJumpDown),
        (&bindings.jump_to_failure, LayoutAction::JumpToFailure),
        (&bindings.zoom_in, LayoutAction::ZoomIn),
        (&bindings.zoom_out, LayoutAction::ZoomOut),
        (&bindings.zoom_reset, LayoutAction::ZoomReset),
    ];

    for (set, expected) in layout_cases {
        let input = pressed_first(set);
        assert_eq!(
            translate_layout_shortcut(&input, &bindings),
            Some(*expected),
            "layout action {expected:?} did not resolve from its default binding",
        );
        assert_eq!(translate_key_action(&input, &bindings), Some(KeyAction::Layout(*expected)));
    }
}

#[test]
fn palette_settings_find_resolve() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    assert_eq!(
        translate_key_action(&pressed_first(&bindings.command_palette), &bindings),
        Some(KeyAction::OpenCommandPalette),
    );
    assert_eq!(
        translate_key_action(&pressed_first(&bindings.settings), &bindings),
        Some(KeyAction::OpenSettings),
    );
    assert_eq!(
        translate_key_action(&pressed_first(&bindings.find), &bindings),
        Some(KeyAction::OpenFind),
    );
}

#[test]
fn terminal_shortcuts_emit_fixed_sequences() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    let cases: &[(&[Keybinding], &[u8])] = &[
        (&bindings.word_left, b"\x1b[1;5D"),
        (&bindings.word_right, b"\x1b[1;5C"),
        (&bindings.delete_word_backward, &[0x1b, 0x7f]),
        (&bindings.delete_word_backward_ctrl, &[0x08]),
        (&bindings.delete_word_forward, b"\x1b[3;5~"),
        (&bindings.line_start, b"\x1b[1;5H"),
        (&bindings.line_end, b"\x1b[1;5F"),
    ];

    for (set, expected) in cases {
        let input = pressed_first(set);
        assert_eq!(
            translate_terminal_shortcut(&input, &bindings).as_deref(),
            Some(*expected),
            "terminal shortcut did not emit its fixed sequence",
        );
        assert_eq!(
            translate_key_action(&input, &bindings),
            Some(KeyAction::Terminal(expected.to_vec())),
        );
    }
}

/// `equalize` is the one layout action the GPUI rebuild shipped without a
/// binding, so its default has to be a combo nothing else in the shipped set
/// already owns. Driving the literal chord proves the choice is unclaimed: any
/// collision would resolve to the other action instead, whichever dispatch
/// table it sits in.
#[test]
fn equalize_default_chord_is_claimed_by_nothing_else() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    assert_eq!(
        KeybindingsConfig::default().equalize.as_slice(),
        ["ctrl+shift+e".to_string()],
        "the equalize default moved; re-check it against the whole configured set",
    );
    assert_eq!(
        translate_key_action(&pressed_combo("ctrl+shift+e"), &bindings),
        Some(KeyAction::Layout(LayoutAction::Equalize)),
    );
}

#[test]
fn non_binding_key_falls_through() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());
    // A bare 'q' with no modifiers matches no configured shortcut, so dispatch
    // returns None and the caller's generic encoder handles it.
    let input = KeyInput {
        token: KeyToken::Char('q'),
        base: Some('q'),
        text: Some("q".into()),
        modifiers: Modifiers::default(),
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
    };
    assert_eq!(translate_key_action(&input, &bindings), None);
}

/// Build a pressed `KeyInput` for a literal combo string.
fn pressed_combo(combo: &str) -> KeyInput {
    input_for(&Keybinding::parse(combo).expect("valid combo"), KeyState::Pressed)
}

#[test]
fn overlay_chords_stay_clear_of_the_default_bindings() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    for (combo, chord) in OVERLAY_CHORDS {
        let input = pressed_combo(combo);
        assert_eq!(
            translate_key_action(&input, &bindings),
            None,
            "{combo} collides with a default binding, so overlay {chord:?} is unreachable",
        );
        assert_eq!(translate_overlay_chord(&input, &bindings), Some(chord));
    }
}

#[test]
fn tab_and_window_chords_are_never_claimed_by_an_overlay() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());

    let cases: &[(&[Keybinding], LayoutAction)] = &[
        (&bindings.close_tab, LayoutAction::CloseTab),
        (&bindings.new_window, LayoutAction::NewWindow),
    ];

    for (set, expected) in cases {
        let input = pressed_first(set);
        assert_eq!(
            translate_overlay_chord(&input, &bindings),
            None,
            "the {expected:?} chord is swallowed by a shell overlay before it can dispatch",
        );
        assert_eq!(translate_key_action(&input, &bindings), Some(KeyAction::Layout(*expected)));
    }
}

#[test]
fn a_rebind_onto_an_overlay_chord_wins() {
    let mut config = KeybindingsConfig::default();
    // Move `close_tab` onto the close-dialog overlay's own chord.
    config.close_tab = scribe_common::config::KeyComboList(vec!["ctrl+shift+d".to_string()]);
    let bindings = Bindings::parse(&config);

    let input = pressed_combo("ctrl+shift+d");
    assert_eq!(translate_overlay_chord(&input, &bindings), None);
    assert_eq!(
        translate_key_action(&input, &bindings),
        Some(KeyAction::Layout(LayoutAction::CloseTab)),
    );
}

#[test]
fn overlay_chords_ignore_key_release() {
    let bindings = Bindings::parse(&KeybindingsConfig::default());
    let binding = Keybinding::parse("ctrl+shift+d").expect("valid combo");
    let released = input_for(&binding, KeyState::Released);
    assert_eq!(translate_overlay_chord(&released, &bindings), None);
}

#[test]
fn invalid_bindings_are_skipped_not_fatal() {
    let mut config = KeybindingsConfig::default();
    config.split_vertical = scribe_common::config::KeyComboList(vec![
        "not-a-key".to_string(),
        "ctrl+shift+backslash".to_string(),
        "ctrl+shift+d".to_string(),
    ]);
    let bindings = Bindings::parse(&config);
    // "not-a-key" and the multi-char "backslash" are skipped; the valid combo
    // remains and resolves.
    assert_eq!(bindings.split_vertical.len(), 1);
    let input = pressed_first(&bindings.split_vertical);
    assert_eq!(translate_layout_shortcut(&input, &bindings), Some(LayoutAction::SplitVertical));
}
