//! Config load, removed-key tolerance, and live-reload tests.
//!
//! Covers the two acceptance criteria this module owns: a config carrying
//! removed appearance keys loads without error and leaves the GPUI-consumed
//! surface intact (`config-load-with-removed-keys`), and a scripted reload
//! confirms edits to theme, font, and keybindings reapply live without a
//! restart.

use std::path::Path;

use scribe_common::config::ScribeConfig;

use super::{ClientConfig, ConfigChangeSignal, ConfigRuntime, is_relevant_config_event_path};

fn parse(toml_src: &str) -> ScribeConfig {
    toml::from_str(toml_src).expect("config TOML parses")
}

/// A config containing every removed appearance key alongside live keys. The
/// removed keys must deserialize harmlessly (serde ignores what it does not
/// model, and the retained-but-inert keys have defaults), while the
/// GPUI-consumed fields still parse.
const REMOVED_KEYS_TOML: &str = r##"
[appearance]
font = "Fira Code"
font_size = 16.0
theme = "dracula"
splash = true
splash_duration_ms = 2500
scrollbar_width = 12.0
scrollbar_color = "#abcdef"
prompt_bar_second_row_bg = "#111111"
prompt_bar_first_row_bg = "#222222"
prompt_bar_text = "#333333"
prompt_bar_icon_first = "#444444"
prompt_bar_icon_latest = "#555555"
"##;

// @lat: [[test#GPUI Client Headless Suites#Config load with removed keys]]
#[gpui::test]
fn config_load_with_removed_keys(_cx: &mut gpui::TestAppContext) {
    // Deserialization must not error despite the removed / inert keys.
    let config = parse(REMOVED_KEYS_TOML);

    // The GPUI-consumed appearance surface parsed correctly and is unaffected
    // by the removed keys.
    assert_eq!(config.appearance.font, "Fira Code");
    assert!((config.appearance.font_size - 16.0).abs() < f32::EPSILON);
    assert_eq!(config.appearance.theme, "dracula");

    // Resolving the full snapshot (theme + chrome + bindings) succeeds; the
    // removed keys are inert and never reach the GPUI paint path.
    let client = ClientConfig::from_config(config);
    assert_eq!(client.theme.name, "dracula");
    assert_eq!(client.chrome, client.theme.chrome);
    assert!(!client.bindings.split_vertical.is_empty());
}

// @lat: [[test#GPUI Client Headless Suites#Config live reload]]
#[gpui::test]
fn reload_reapplies_theme_font_and_keybindings(_cx: &mut gpui::TestAppContext) {
    let initial = parse(
        r#"
[appearance]
font = "JetBrains Mono"
font_size = 14.0
theme = "minimal-dark"

[keybindings]
split_vertical = ["ctrl+shift+d"]
"#,
    );
    let mut client = ClientConfig::from_config(initial);
    let old_theme = client.theme.clone();
    let old_split = client.bindings.split_vertical.clone();

    let edited = parse(
        r#"
[appearance]
font = "Fira Code"
font_size = 18.0
theme = "dracula"

[keybindings]
split_vertical = ["ctrl+shift+e"]
"#,
    );
    let plan = client.reload(edited);

    // Theme reapplied live.
    assert!(plan.theme_changed());
    assert_ne!(client.theme, old_theme);
    assert_eq!(client.theme.name, "dracula");
    assert_eq!(client.chrome, client.theme.chrome);

    // Font metrics reapplied live (drives a layout resize downstream).
    assert!(plan.font_changed());
    assert_eq!(client.config.appearance.font, "Fira Code");
    assert!((client.config.appearance.font_size - 18.0).abs() < f32::EPSILON);

    // Keybindings reapplied live: the old combo no longer matches, the new one
    // does.
    let new_split = &client.bindings.split_vertical;
    assert_ne!(format!("{new_split:?}"), format!("{old_split:?}"));
    let new_binding = new_split.first().expect("has binding");
    assert!(new_binding.modifiers.control && new_binding.modifiers.shift);
    assert_eq!(new_binding.key, crate::keybindings::KeyMatch::Character('e'));

    assert!(plan.any_changed());
}

#[gpui::test]
fn reload_with_only_opacity_change_is_scoped(_cx: &mut gpui::TestAppContext) {
    let base = parse("[appearance]\nopacity = 1.0\n");
    let mut client = ClientConfig::from_config(base);

    let dimmed = parse("[appearance]\nopacity = 0.8\n");
    let plan = client.reload(dimmed);

    assert!(plan.opacity_changed());
    assert!(!plan.theme_changed());
    assert!(!plan.font_changed());
}

#[gpui::test]
fn reload_with_no_changes_reports_nothing(_cx: &mut gpui::TestAppContext) {
    let cfg = parse("[appearance]\ntheme = \"minimal-dark\"\n");
    let mut client = ClientConfig::from_config(cfg.clone());
    let plan = client.reload(cfg);
    assert!(!plan.any_changed());
}

#[test]
fn watcher_relevance_matches_config_and_theme_paths() {
    let dir = Path::new("/home/user/.config/scribe");
    assert!(is_relevant_config_event_path(dir, &dir.join("config.toml")));
    assert!(is_relevant_config_event_path(dir, &dir.join("themes/my-theme.toml")));
    assert!(!is_relevant_config_event_path(dir, &dir.join("other.txt")));
}

// @lat: [[test#GPUI Client Headless Suites#Config live reload#Watcher signal collapses a burst]]
#[test]
fn change_signal_collapses_a_save_burst_into_one_reload() {
    let signal = ConfigChangeSignal::new();
    let mut seen = signal.generation();

    // Nothing written yet: no reload is due.
    assert!(!signal.take_change(&mut seen));

    // One editor save fires several notify events (delete, create, modify).
    signal.signal();
    signal.signal();
    signal.signal();

    // They collapse into exactly one reload, and the flag then clears.
    assert!(signal.take_change(&mut seen));
    assert!(!signal.take_change(&mut seen));
}

// @lat: [[test#GPUI Client Headless Suites#Config live reload#Runtime applies a watcher-signalled edit]]
#[test]
fn runtime_reloads_only_after_the_watcher_signals() {
    let initial = parse(
        r#"
[appearance]
font = "JetBrains Mono"
font_size = 14.0
theme = "minimal-dark"
opacity = 1.0

[keybindings]
command_palette = ["ctrl+shift+p"]
"#,
    );
    let mut runtime = ConfigRuntime::detached(ClientConfig::from_config(initial));
    assert_eq!(runtime.config().theme.name, "minimal-dark");

    // No watcher event yet, so the foreground poll must not reload.
    assert!(!runtime.take_pending());

    // The watcher fires; the foreground now has a reload to run.
    let signal = runtime.signal();
    signal.signal();
    assert!(runtime.take_pending());

    // Applying the edited file swaps every live surface in one step.
    let plan = runtime.reload(parse(
        r#"
[appearance]
font = "Fira Code"
font_size = 18.0
theme = "dracula"
opacity = 0.85

[keybindings]
command_palette = ["ctrl+shift+o"]
"#,
    ));

    assert!(plan.theme_changed());
    assert!(plan.font_changed());
    assert!(plan.opacity_changed());
    assert_eq!(runtime.config().theme.name, "dracula");
    assert_eq!(runtime.config().config.appearance.font, "Fira Code");
    assert!((runtime.opacity() - 0.85).abs() < f32::EPSILON);

    // Keybindings are re-parsed unconditionally, so the new palette combo is
    // live without a restart and the old one no longer matches.
    let combo = runtime.bindings().command_palette.first().expect("palette combo parsed");
    assert_eq!(combo.key, crate::keybindings::KeyMatch::Character('o'));

    // The signal was consumed by the earlier poll: no second reload is queued.
    assert!(!runtime.take_pending());
}
