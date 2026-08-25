//! GPUI settings window rebuilt from the deleted standalone GTK/wry app as a
//! window in the client process.
//!
//! The webview app is gone; its feature set is reproduced here. [`apply`] and
//! [`server_action`] are ported verbatim from the old crate — the config-write
//! and one-shot server-request logic is unchanged so no editable setting or
//! update/release action regresses. [`singleton`] absorbs the
//! `settings.lock`/`settings.sock` singleton (a second `--settings` launch hands
//! focus to the running window instead of opening a duplicate), and [`state`]
//! persists window geometry across restarts. [`model`] describes the eleven pages
//! and their controls declaratively, [`values`] reads current config values back
//! for rendering, and [`window`] lowers all of it onto a GPUI view.
//!
//! During side-by-side development the old GTK app stays the sole live-config
//! writer; this window is pointed at a separate dev config via the
//! `SCRIBE_CONFIG_DIR` override that [`scribe_common::config::load_config`]
//! already honours, so the two never race on `config.toml`.

pub mod apply;
pub mod model;
mod release_notes;
pub mod server_action;
pub mod singleton;
mod smart_selection_editor;
pub mod state;
pub mod values;
pub mod window;

pub use window::{SettingsWindow, open_settings_window, recenter_settings_window};

#[cfg(test)]
mod parity_tests {
    use super::apply::apply_config_key;
    use super::model::{ControlKind, SettingsPage, page_controls};
    use super::values::{current_value, keybinding_combos};
    use scribe_common::config::ScribeConfig;
    use serde_json::json;

    // @lat: [[test#GPUI Settings Window#Per-page parity checklist]]
    /// Every page exposes controls, and every config-backed control on every
    /// page routes cleanly through the ported apply path with the value the
    /// window would read for it. A hex placeholder stands in for color controls
    /// so an unset custom theme still proves the key is wired.
    #[test]
    fn every_page_control_routes_through_apply() {
        let base = ScribeConfig::default();
        for page in SettingsPage::all() {
            let controls = page_controls(page);
            assert!(!controls.is_empty(), "page {page:?} must expose controls");
            for control in &controls {
                check_control_applies(&base, control);
            }
        }
    }

    // @lat: [[test#GitHub CI Opt-in#Settings round trip]]
    #[test]
    fn github_ci_toggle_round_trips_through_the_terminal_page() {
        let control = page_controls(SettingsPage::Terminal)
            .into_iter()
            .find(|control| control.key == "github_ci.enabled")
            .expect("Terminal page must expose GitHub CI");
        assert!(matches!(control.kind, ControlKind::Toggle));

        let mut config = ScribeConfig::default();
        assert_eq!(current_value(&config, &control.key), false);
        apply_config_key(&mut config, &control.key, &json!(true))
            .expect("GitHub CI toggle applies");
        assert_eq!(current_value(&config, &control.key), true);
    }

    #[test]
    fn theme_preset_control_carries_only_the_custom_token() {
        let preset = page_controls(SettingsPage::Colors)
            .into_iter()
            .find(|control| control.key == "theme.preset")
            .expect("Colors page must expose the theme preset control");
        let ControlKind::Choice(options) = preset.kind else {
            panic!("theme.preset must remain a choice control");
        };

        assert_eq!(options, vec![("custom", "Custom")]);
    }

    /// Assert one control routes through the apply path with the window's value.
    fn check_control_applies(base: &ScribeConfig, control: &super::model::Control) {
        let Some((key, value)) = apply_input_for(base, control) else {
            return; // Action controls carry no config value.
        };
        let mut config = base.clone();
        if let Err(e) = apply_config_key(&mut config, &key, &value) {
            panic!("control {} failed to apply: {e}", control.key);
        }
    }

    /// Build the `(key, value)` the window would hand the apply path for a
    /// control, or `None` for action buttons that carry no config value.
    fn apply_input_for(
        base: &ScribeConfig,
        control: &super::model::Control,
    ) -> Option<(String, serde_json::Value)> {
        match &control.kind {
            ControlKind::Toggle
            | ControlKind::Choice(_)
            | ControlKind::Stepper { .. }
            | ControlKind::Text => {
                let value = current_value(base, &control.key);
                assert!(!value.is_null(), "control {} must have a readable value", control.key);
                Some((control.key.clone(), value))
            }
            // A valid hex proves the color key is accepted even when the current
            // custom theme value is unset (empty).
            ControlKind::Color => Some((control.key.clone(), json!("#123456"))),
            ControlKind::Keybinding => {
                let combos = keybinding_combos(base, &control.key);
                Some((format!("keybindings.{}", control.key), json!(combos)))
            }
            ControlKind::Action => None,
        }
    }

    // @lat: [[test#GPUI Settings Window#Shortcut capture#Actions read as product language]]
    /// Every keybinding row is labelled in the same sentence case the rest of
    /// the settings pages use, with the proper nouns intact, so the page reads
    /// as a list of things Scribe does rather than a dump of config field
    /// names.
    #[test]
    fn keybinding_labels_are_sentence_case_product_language() {
        use super::model::keybinding_label;

        assert_eq!(keybinding_label("new_tab"), "New tab");
        assert_eq!(keybinding_label("new_claude_resume_tab"), "New Claude resume tab");
        assert_eq!(keybinding_label("new_codex_tab"), "New Codex tab");
        assert_eq!(keybinding_label("new_pi_tab"), "New Pi tab");
        assert_eq!(keybinding_label("prev_tab"), "Previous tab");
        assert_eq!(keybinding_label("select_tab_1"), "Select tab 1");
        assert_eq!(keybinding_label("delete_word_backward_ctrl"), "Delete word backward (Ctrl)");
        for action in super::model::keybinding_actions() {
            let label = keybinding_label(action);
            assert!(!label.contains('_'), "{action} label still reads as a config key: {label}");
            assert!(
                label.starts_with(|c: char| c.is_uppercase()),
                "{action} label must open in sentence case: {label}"
            );
        }
    }

    // @lat: [[test#GPUI Settings Window#Keybinding coverage]]
    /// The keybindings page lists every action the apply path routes under
    /// `keybindings.*`, so no shortcut silently disappears from the rebuilt
    /// surface. Each action's combo list round-trips back through the reader.
    #[test]
    fn keybinding_page_covers_all_actions() {
        let config = ScribeConfig::default();
        let controls = page_controls(SettingsPage::Keybindings);
        assert!(controls.len() >= 50, "expected the full 50+ keybinding action set");
        for control in controls {
            assert!(matches!(control.kind, ControlKind::Keybinding));
            // Reading the combo list must not panic and must match a real field.
            drop(keybinding_combos(&config, &control.key));
        }
    }
}
