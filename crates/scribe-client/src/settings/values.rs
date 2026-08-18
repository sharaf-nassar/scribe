//! Current-value reader for the GPUI settings window.
//!
//! The window renders each [`crate::settings::model::Control`] against the live
//! [`scribe_common::config::ScribeConfig`] and, for the interactive kinds,
//! needs the current value to compute the toggled/cycled/stepped next value it
//! hands to [`crate::settings::apply::apply_settings_change`]. This module is
//! the read side of that round-trip: it maps a UI control key back to its
//! current config value as a [`serde_json::Value`], the same shape the apply
//! path consumes. Keys the reader does not recognise yield
//! [`serde_json::Value::Null`] so the window can fall back to a neutral render.

use scribe_common::config::ScribeConfig;
use serde_json::{Value, json};

/// Read the current value of a control `key` from `config`.
///
/// Booleans come back as [`Value::Bool`], numeric steppers as [`Value::Number`],
/// and choice/color/text controls as [`Value::String`]. Enum fields are routed
/// through `serde_json::to_value` so the returned token matches exactly what the
/// apply path accepts. Unknown keys return [`Value::Null`].
#[must_use]
pub fn current_value(config: &ScribeConfig, key: &str) -> Value {
    if let Some(v) = appearance_value(config, key) {
        return v;
    }
    if let Some(v) = terminal_value(config, key) {
        return v;
    }
    if let Some(v) = ai_value(config, key) {
        return v;
    }
    if let Some(v) = misc_value(config, key) {
        return v;
    }
    Value::Null
}

fn enum_str<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn opt_color(value: Option<&str>) -> Value {
    Value::String(value.unwrap_or("").to_owned())
}

fn appearance_value(config: &ScribeConfig, key: &str) -> Option<Value> {
    let a = &config.appearance;
    let v = match key {
        "appearance.font_family" => Value::String(a.font.clone()),
        "appearance.font_size" => json!(a.font_size),
        "appearance.font_weight" => json!(a.font_weight),
        "appearance.bold_weight" => json!(a.font_weight_bold),
        "appearance.ligatures" => Value::Bool(a.ligatures),
        "appearance.line_padding" => json!(a.line_padding),
        "appearance.cursor_shape" => enum_str(&a.cursor_shape),
        "appearance.cursor_blink" => Value::Bool(a.cursor_blink),
        "appearance.opacity" => json!(a.opacity),
        "appearance.scrollbar_width" => json!(a.scrollbar_width),
        "appearance.tab_bar_padding" => json!(a.tab_bar_padding),
        "appearance.tab_width" => json!(a.tab_width),
        "appearance.status_bar_height" => json!(a.status_bar_height),
        "appearance.tab_height" => json!(a.tab_height),
        "appearance.focus_border_width" => json!(a.focus_border_width),
        "appearance.focus_border_color" => opt_color(a.focus_border_color.as_deref()),
        "appearance.prompt_bar_first_row_bg" => opt_color(a.prompt_bar_first_row_bg.as_deref()),
        "appearance.prompt_bar_second_row_bg" => opt_color(a.prompt_bar_second_row_bg.as_deref()),
        "appearance.prompt_bar_text" => opt_color(a.prompt_bar_text.as_deref()),
        "appearance.prompt_bar_icon_first" => opt_color(a.prompt_bar_icon_first.as_deref()),
        "appearance.prompt_bar_icon_latest" => opt_color(a.prompt_bar_icon_latest.as_deref()),
        "theme.preset" => Value::String(a.theme.clone()),
        _ => return theme_color_value(config, key),
    };
    Some(v)
}

fn theme_color_value(config: &ScribeConfig, key: &str) -> Option<Value> {
    if !key.starts_with("theme.") {
        return None;
    }
    let resolved;
    let (foreground, background, cursor, cursor_accent, selection, selection_foreground, colors) =
        if let Some(theme) = config.theme.as_ref() {
            (
                theme.foreground.clone(),
                theme.background.clone(),
                theme.cursor.clone(),
                theme.cursor_accent.clone(),
                theme.selection.clone(),
                theme.selection_foreground.clone(),
                theme.colors.clone(),
            )
        } else {
            resolved = scribe_common::config::resolve_theme(config);
            (
                scribe_common::theme::rgba_to_hex(resolved.foreground),
                scribe_common::theme::rgba_to_hex(resolved.background),
                scribe_common::theme::rgba_to_hex(resolved.cursor),
                scribe_common::theme::rgba_to_hex(resolved.cursor_accent),
                scribe_common::theme::rgba_to_hex(resolved.selection),
                scribe_common::theme::rgba_to_hex(resolved.selection_foreground),
                resolved
                    .ansi_colors
                    .iter()
                    .map(|color| scribe_common::theme::rgba_to_hex(*color))
                    .collect(),
            )
        };
    let v = match key {
        "theme.foreground" => foreground,
        "theme.background" => background,
        "theme.cursor" => cursor,
        "theme.cursor_text" => cursor_accent,
        "theme.selection" => selection,
        "theme.selection_text" => selection_foreground,
        _ => {
            if let Some(idx) = key.strip_prefix("theme.ansi_normal.").and_then(|s| s.parse().ok()) {
                colors.get::<usize>(idx).cloned().unwrap_or_default()
            } else if let Some(idx) =
                key.strip_prefix("theme.ansi_bright.").and_then(|s| s.parse::<usize>().ok())
            {
                colors.get(idx + 8).cloned().unwrap_or_default()
            } else {
                return Some(Value::Null);
            }
        }
    };
    Some(Value::String(v))
}

fn terminal_value(config: &ScribeConfig, key: &str) -> Option<Value> {
    let t = &config.terminal;
    let v = match key {
        "terminal.scrollback_lines" => json!(t.scrollback_lines),
        "terminal.copy_on_select" => Value::Bool(t.clipboard.copy_on_select),
        "terminal.claude_copy_cleanup" => Value::Bool(t.clipboard.claude_copy_cleanup),
        "terminal.natural_scroll" => Value::Bool(t.scroll.natural_scroll),
        "terminal.focus_follows_mouse" => Value::Bool(t.focus.focus_follows_mouse),
        "terminal.keyboard_protocol_enhanced" => Value::Bool(t.keyboard_protocol_enhanced),
        "terminal.paste_confirmation" => Value::Bool(t.paste_confirmation),
        "terminal.images.enabled" => Value::Bool(t.images.enabled),
        "terminal.env_persistence.enabled" => Value::Bool(t.env_persistence.enabled),
        "terminal.claude_code_integration" => Value::Bool(t.ai_integration.claude_code.enabled()),
        "terminal.codex_code_integration" => Value::Bool(t.ai_integration.codex_code.enabled()),
        "terminal.pi_integration" => Value::Bool(t.ai_integration.pi.enabled()),
        "terminal.prompt_bar" => Value::Bool(t.prompt_bar.enabled),
        // Unset, the strip follows the terminal font, so the stepper opens on
        // the size the user is actually looking at rather than a stale default.
        "terminal.prompt_bar_font_size" => {
            json!(t.prompt_bar.font_size.unwrap_or(config.appearance.font_size))
        }
        "terminal.prompt_bar_position" => enum_str(&t.prompt_bar.position),
        "terminal.preserve_ai_scrollback" => Value::Bool(t.ai_session.preserve_ai_scrollback),
        "terminal.indicator_height" => json!(t.ai_session.indicator_height),
        "terminal.status_bar_stats.cpu" => Value::Bool(t.status_bar_stats.usage.compute.cpu),
        "terminal.status_bar_stats.memory" => Value::Bool(t.status_bar_stats.usage.memory),
        "terminal.status_bar_stats.gpu" => Value::Bool(t.status_bar_stats.usage.compute.gpu),
        "terminal.status_bar_stats.network" => Value::Bool(t.status_bar_stats.network),
        "terminal.clipboard.read_mode" => enum_str(&t.clipboard_policy.read_mode),
        "terminal.clipboard.write_mode" => enum_str(&t.clipboard_policy.write_mode),
        "terminal.clipboard.max_write_bytes" => json!(t.clipboard_policy.max_write_bytes),
        "terminal.clipboard.focus_gate_writes" => Value::Bool(t.clipboard_policy.focus_gate_writes),
        _ => return None,
    };
    Some(v)
}

fn ai_value(config: &ScribeConfig, key: &str) -> Option<Value> {
    let rest = key.strip_prefix("ai_states.").or_else(|| key.strip_prefix("claude_states."))?;
    let (state, field) = rest.split_once('.')?;
    let states = &config.terminal.ai_session.ai_states;
    let entry = match state {
        "processing" => &states.processing,
        "idle_prompt" | "waiting_for_input" => &states.waiting_for_input,
        "permission_prompt" => &states.permission_prompt,
        "error" => &states.error,
        _ => return Some(Value::Null),
    };
    let v = match field {
        "tab_indicator" => Value::Bool(entry.tab_indicator),
        "pane_border" => Value::Bool(entry.pane_border),
        "color" => enum_str(&entry.color),
        "pulse_ms" => json!(entry.pulse_ms),
        "timeout_secs" => json!(entry.timeout_secs),
        _ => Value::Null,
    };
    Some(v)
}

fn misc_value(config: &ScribeConfig, key: &str) -> Option<Value> {
    if let Some(index) =
        key.strip_prefix("workspaces.badge_colors.").and_then(|index| index.parse::<usize>().ok())
    {
        return Some(
            config.workspaces.badge_colors.get(index).cloned().map_or(Value::Null, Value::String),
        );
    }
    let v = match key {
        "github_ci.enabled" => Value::Bool(config.github_ci.enabled),
        "update.enabled" => Value::Bool(config.update.enabled),
        "update.check_interval_hours" => json!(config.update.check_interval_secs / 3600),
        "update.channel" => enum_str(&config.update.channel),
        "notifications.enabled" => Value::Bool(config.notifications.enabled),
        "notifications.condition" => enum_str(&config.notifications.condition),
        "notifications.timeout_mode" => enum_str(&config.notifications.timeout_mode),
        "notifications.timeout_secs" => json!(config.notifications.timeout_secs),
        "remote.enabled" => Value::Bool(config.remote.enabled),
        "remote.port" => json!(config.remote.port),
        "remote.lan.enabled" => Value::Bool(config.remote.lan.enabled),
        "remote.lan.port" => json!(config.remote.lan.port),
        "remote.sharing_mode" => enum_str(&config.remote.sharing_mode),
        "remote.control_acquisition" => enum_str(&config.remote.control_acquisition),
        "remote.participant_limit" => json!(config.remote.participant_limit.unwrap_or(0)),
        _ => return None,
    };
    Some(v)
}

/// Read the current key-combo list for a keybinding `action`.
///
/// [`scribe_common::config::KeybindingsConfig`] serializes each action under a
/// field whose name is exactly the action string the UI keys on, so the combo
/// list is read by serializing the struct once and indexing it — avoiding a
/// 50-arm getter that would drift from the apply path's setter.
#[must_use]
pub fn keybinding_combos(config: &ScribeConfig, action: &str) -> Vec<String> {
    let Ok(Value::Object(map)) = serde_json::to_value(&config.keybindings) else {
        return Vec::new();
    };
    match map.get(action) {
        Some(Value::Array(items)) => {
            items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
        }
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::current_value;

    #[test]
    fn reads_dynamic_workspace_badge_colors() {
        let mut config = scribe_common::config::ScribeConfig::default();
        config.workspaces.badge_colors = vec!["#112233".to_owned(), "#abcdef".to_owned()];

        assert_eq!(current_value(&config, "workspaces.badge_colors.1"), "#abcdef");
        assert!(current_value(&config, "workspaces.badge_colors.2").is_null());
        assert!(current_value(&config, "workspaces.badge_colors.nope").is_null());
    }

    #[test]
    fn reads_focus_follows_mouse_value() {
        let mut config = scribe_common::config::ScribeConfig::default();
        assert_eq!(current_value(&config, "terminal.focus_follows_mouse"), true);

        config.terminal.focus.focus_follows_mouse = false;
        assert_eq!(current_value(&config, "terminal.focus_follows_mouse"), false);
    }
}
