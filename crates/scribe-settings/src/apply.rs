//! Config change application logic.
//!
//! Parses settings change messages from the webview and applies them to
//! the config file on disk.

/// Apply a single settings change from the webview to the config file.
///
/// Parses the JSON change message, loads the current config, applies the
/// change, and writes the updated config back. The file watcher will detect
/// the change and trigger a `ConfigChanged` event.
pub fn apply_settings_change(change_json: &str) -> Result<(), String> {
    let msg: serde_json::Value =
        serde_json::from_str(change_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let key = msg
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| String::from("missing 'key' field"))?;

    let value = msg.get("value").ok_or_else(|| String::from("missing 'value' field"))?;

    let mut config =
        scribe_common::config::load_config().map_err(|e| format!("failed to load config: {e}"))?;

    apply_config_key(&mut config, key, value)?;

    scribe_common::config::save_config(&config).map_err(|e| format!("failed to save config: {e}"))
}

/// Apply a single dotted key + value to the config struct.
#[allow(clippy::too_many_lines, reason = "exhaustive key matching requires one arm per setting")]
fn apply_config_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        // -- Appearance -------------------------------------------------------
        "appearance.font_family" => {
            value
                .as_str()
                .ok_or("font_family must be a string")?
                .clone_into(&mut config.appearance.font);
        }
        "appearance.font_size" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "font_size is a small positive float"
            )]
            let v = value.as_f64().ok_or("font_size must be a number")? as f32;
            config.appearance.font_size = v;
        }
        "appearance.font_weight" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "font weight is a small positive integer (100-900)"
            )]
            let v = value.as_f64().ok_or("font_weight must be a number")? as u16;
            config.appearance.font_weight = v;
        }
        "appearance.bold_weight" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "bold weight is a small positive integer (100-900)"
            )]
            let v = value.as_f64().ok_or("bold_weight must be a number")? as u16;
            config.appearance.font_weight_bold = v;
        }
        "appearance.ligatures" => {
            config.appearance.ligatures = value.as_bool().ok_or("ligatures must be a boolean")?;
        }
        "appearance.line_padding" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "line padding is a small non-negative integer"
            )]
            let v = value.as_f64().ok_or("line_padding must be a number")? as u16;
            config.appearance.line_padding = v;
        }
        "appearance.cursor_shape" => {
            let shape_str = value.as_str().ok_or("cursor_shape must be a string")?;
            let shape: scribe_common::config::CursorShape =
                serde_json::from_value(serde_json::Value::String(shape_str.to_owned()))
                    .map_err(|e| format!("invalid cursor shape: {e}"))?;
            config.appearance.cursor_shape = shape;
        }
        "appearance.cursor_blink" => {
            config.appearance.cursor_blink =
                value.as_bool().ok_or("cursor_blink must be a boolean")?;
        }
        "appearance.opacity" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "opacity is a float between 0.0 and 1.0"
            )]
            let v = value.as_f64().ok_or("opacity must be a number")? as f32;
            config.appearance.opacity = v;
        }
        "appearance.scrollbar_width" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "scrollbar width is a small positive float"
            )]
            let v = value.as_f64().ok_or("scrollbar_width must be a number")? as f32;
            config.appearance.scrollbar_width = v.clamp(2.0, 20.0);
        }
        "appearance.tab_bar_padding" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "tab bar padding is a small non-negative float"
            )]
            let v = value.as_f64().ok_or("tab_bar_padding must be a number")? as f32;
            config.appearance.tab_bar_padding = v.clamp(0.0, 20.0);
        }
        "appearance.tab_width" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "tab width is a small positive integer within u16 range"
            )]
            let v = value.as_f64().ok_or("tab_width must be a number")? as u16;
            config.appearance.tab_width = v.clamp(8, 50);
        }
        "appearance.status_bar_height" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "status bar height is a small positive float"
            )]
            let v = value.as_f64().ok_or("status_bar_height must be a number")? as f32;
            config.appearance.status_bar_height = v.clamp(8.0, 48.0);
        }
        "appearance.tab_height" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "tab height is a small positive float"
            )]
            let v = value.as_f64().ok_or("tab_height must be a number")? as f32;
            config.appearance.tab_height = v.clamp(16.0, 60.0);
        }
        "appearance.content_padding_top" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "content padding top is a small non-negative float"
            )]
            let v = value.as_f64().ok_or("content_padding_top must be a number")? as f32;
            config.appearance.content_padding.top = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_right" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "content padding right is a small non-negative float"
            )]
            let v = value.as_f64().ok_or("content_padding_right must be a number")? as f32;
            config.appearance.content_padding.right = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_bottom" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "content padding bottom is a small non-negative float"
            )]
            let v = value.as_f64().ok_or("content_padding_bottom must be a number")? as f32;
            config.appearance.content_padding.bottom = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_left" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "content padding left is a small non-negative float"
            )]
            let v = value.as_f64().ok_or("content_padding_left must be a number")? as f32;
            config.appearance.content_padding.left = v.clamp(0.0, 50.0);
        }
        "appearance.focus_border_color" => {
            let hex = value.as_str().ok_or("focus_border_color must be a string")?;
            if hex.is_empty() {
                config.appearance.focus_border_color = None;
            } else {
                scribe_common::theme::hex_to_rgba(hex)
                    .map_err(|e| format!("invalid focus_border_color: {e}"))?;
                config.appearance.focus_border_color = Some(hex.to_owned());
            }
        }
        "appearance.focus_border_width" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "focus border width is a small positive float"
            )]
            let v = value.as_f64().ok_or("focus_border_width must be a number")? as f32;
            config.appearance.focus_border_width = v.clamp(1.0, 10.0);
        }
        // -- Theme preset -----------------------------------------------------
        "theme.preset" => {
            let preset = value.as_str().ok_or("theme preset must be a string")?;
            // Convert preset name: "minimal_dark" -> "minimal-dark"
            config.appearance.theme = preset.replace('_', "-");
            if preset != "custom" {
                config.theme = None;
            }
        }
        // -- Terminal ---------------------------------------------------------
        "terminal.scrollback_lines" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "scrollback is a non-negative integer within u32 range"
            )]
            let v = value.as_f64().ok_or("scrollback_lines must be a number")? as u32;
            config.terminal.scrollback_lines = v;
        }
        "terminal.copy_on_select" => {
            config.terminal.copy_on_select =
                value.as_bool().ok_or("copy_on_select must be a boolean")?;
        }
        "terminal.claude_copy_cleanup" => {
            config.terminal.claude_copy_cleanup =
                value.as_bool().ok_or("claude_copy_cleanup must be a boolean")?;
        }
        "terminal.claude_code_integration" => {
            config.terminal.claude_code_integration =
                value.as_bool().ok_or("claude_code_integration must be a boolean")?;
        }
        "terminal.codex_code_integration" => {
            config.terminal.codex_code_integration =
                value.as_bool().ok_or("codex_code_integration must be a boolean")?;
        }
        "terminal.hide_codex_hook_logs" => {
            config.terminal.hide_codex_hook_logs =
                value.as_bool().ok_or("hide_codex_hook_logs must be a boolean")?;
        }
        "terminal.prompt_bar" => {
            config.terminal.prompt_bar = value.as_bool().ok_or("prompt_bar must be a boolean")?;
        }
        "terminal.ai_tab_provider" => {
            let provider_str = value.as_str().ok_or("ai_tab_provider must be a string")?;
            let provider: scribe_common::ai_state::AiProvider =
                serde_json::from_value(serde_json::Value::String(provider_str.to_owned()))
                    .map_err(|e| format!("invalid ai_tab_provider: {e}"))?;
            config.terminal.ai_tab_provider = provider;
        }
        "terminal.natural_scroll" => {
            config.terminal.natural_scroll =
                value.as_bool().ok_or("natural_scroll must be a boolean")?;
        }
        "terminal.indicator_height" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "indicator height is a small positive float"
            )]
            let v = value.as_f64().ok_or("indicator_height must be a number")? as f32;
            config.terminal.indicator_height = v.clamp(1.0, 10.0);
        }
        "terminal.status_bar_stats.cpu" => {
            config.terminal.status_bar_stats.cpu =
                value.as_bool().ok_or("status_bar_stats.cpu must be a boolean")?;
        }
        "terminal.status_bar_stats.memory" => {
            config.terminal.status_bar_stats.memory =
                value.as_bool().ok_or("status_bar_stats.memory must be a boolean")?;
        }
        "terminal.status_bar_stats.gpu" => {
            config.terminal.status_bar_stats.gpu =
                value.as_bool().ok_or("status_bar_stats.gpu must be a boolean")?;
        }
        "terminal.status_bar_stats.network" => {
            config.terminal.status_bar_stats.network =
                value.as_bool().ok_or("status_bar_stats.network must be a boolean")?;
        }
        // -- Claude States ----------------------------------------------------
        key if key.starts_with("claude_states.") => {
            apply_claude_state_key(&mut config.terminal.claude_states, key, value)?;
        }
        // -- Keybindings ----------------------------------------------------
        key if key.starts_with("keybindings.") => {
            let action = key.trim_start_matches("keybindings.");
            let combos: Vec<String> = match &value {
                serde_json::Value::String(s) => vec![s.clone()],
                serde_json::Value::Array(arr) => {
                    arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
                }
                _ => return Err(String::from("keybinding value must be a string or array")),
            };
            apply_keybinding_field(&mut config.keybindings, action, combos);
        }
        // -- Workspaces -------------------------------------------------------
        "workspaces.add_root" => {
            // The webview sends an empty string as a placeholder; in a real
            // implementation a file picker dialog would provide the path.
            tracing::debug!("workspace add_root requested (file picker not yet implemented)");
        }
        "workspaces.remove_root" => {
            let path = value.as_str().ok_or("remove_root value must be a string")?;
            config.workspaces.roots.retain(|r| r != path);
        }
        "workspaces.reset_badge_colors" => {
            config.workspaces.badge_colors =
                scribe_common::config::WorkspacesConfig::default().badge_colors;
        }
        key if key.starts_with("workspaces.badge_colors.") => {
            let index_str = key.trim_start_matches("workspaces.badge_colors.");
            let index: usize =
                index_str.parse().map_err(|_| String::from("invalid badge color index"))?;
            let color = value.as_str().ok_or("badge color must be a string")?;
            if let Some(slot) = config.workspaces.badge_colors.get_mut(index) {
                color.clone_into(slot);
            }
        }
        // -- Updates ----------------------------------------------------------
        "update.enabled" => {
            let v = value.as_bool().ok_or("update.enabled must be a boolean")?;
            config.update.enabled = v;
        }
        "update.check_interval_hours" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "check interval hours is a small positive integer within u64 range"
            )]
            let hours = (value.as_f64().ok_or("update.check_interval_hours must be a number")?
                as u64)
                .clamp(1, 168);
            config.update.check_interval_secs = hours * 3600;
        }
        "update.channel" => {
            let s = value.as_str().ok_or("update.channel must be a string")?;
            config.update.channel = match s {
                "stable" => scribe_common::config::UpdateChannel::Stable,
                "beta" => scribe_common::config::UpdateChannel::Beta,
                other => return Err(format!("unknown channel: {other}")),
            };
        }
        key if key.starts_with("theme.") => {
            apply_theme_color_key(config, key, value)?;
        }
        _ => {
            tracing::debug!(key, "unhandled settings key");
        }
    }

    Ok(())
}

/// Apply a `claude_states.<state>.<field>` settings change.
fn apply_claude_state_key(
    states: &mut scribe_common::config::ClaudeStatesConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    // Key format: "claude_states.<state>.<field>"
    let rest = key
        .strip_prefix("claude_states.")
        .ok_or_else(|| format!("invalid claude_states key: {key}"))?;
    let (state_name, field) =
        rest.split_once('.').ok_or_else(|| format!("invalid claude_states key: {key}"))?;

    let entry = match state_name {
        "processing" => &mut states.processing,
        "idle_prompt" | "waiting_for_input" => &mut states.waiting_for_input,
        "permission_prompt" => &mut states.permission_prompt,
        "error" => &mut states.error,
        _ => return Err(format!("unknown claude state: {state_name}")),
    };

    match field {
        "tab_indicator" => {
            entry.tab_indicator = value.as_bool().ok_or("tab_indicator must be a boolean")?;
        }
        "pane_border" => {
            entry.pane_border = value.as_bool().ok_or("pane_border must be a boolean")?;
        }
        "color" => {
            let color_str = value.as_str().ok_or("color must be a string")?;
            entry.color = serde_json::from_value(serde_json::Value::String(color_str.to_owned()))
                .map_err(|e| format!("invalid color: {e}"))?;
        }
        "pulse_ms" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "pulse duration is a small non-negative integer"
            )]
            let v = value.as_f64().ok_or("pulse_ms must be a number")? as u32;
            entry.pulse_ms = v;
        }
        "timeout_secs" => {
            #[allow(clippy::cast_possible_truncation, reason = "timeout is a small positive float")]
            let v = value.as_f64().ok_or("timeout_secs must be a number")? as f32;
            entry.timeout_secs = v.max(0.0);
        }
        _ => return Err(format!("unknown claude state field: {field}")),
    }

    Ok(())
}

/// Route a keybinding action name + combo list to the correct config field.
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive keybinding action routing requires one arm per action"
)]
fn apply_keybinding_field(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    combos: Vec<String>,
) {
    use scribe_common::config::KeyComboList;
    let list = KeyComboList::from_vec(combos);
    match action {
        "split_vertical" => kb.split_vertical = list,
        "split_horizontal" => kb.split_horizontal = list,
        "close_pane" => kb.close_pane = list,
        "cycle_pane" => kb.cycle_pane = list,
        "focus_left" => kb.focus_left = list,
        "focus_right" => kb.focus_right = list,
        "focus_up" => kb.focus_up = list,
        "focus_down" => kb.focus_down = list,
        "workspace_split_vertical" => kb.workspace_split_vertical = list,
        "workspace_split_horizontal" => kb.workspace_split_horizontal = list,
        "workspace_focus_left" => kb.workspace_focus_left = list,
        "workspace_focus_right" => kb.workspace_focus_right = list,
        "workspace_focus_up" => kb.workspace_focus_up = list,
        "workspace_focus_down" => kb.workspace_focus_down = list,
        "new_tab" => kb.new_tab = list,
        "new_claude_tab" => kb.new_claude_tab = list,
        "new_claude_resume_tab" => kb.new_claude_resume_tab = list,
        "close_tab" => kb.close_tab = list,
        "next_tab" => kb.next_tab = list,
        "prev_tab" => kb.prev_tab = list,
        "select_tab_1" => kb.select_tab_1 = list,
        "select_tab_2" => kb.select_tab_2 = list,
        "select_tab_3" => kb.select_tab_3 = list,
        "select_tab_4" => kb.select_tab_4 = list,
        "select_tab_5" => kb.select_tab_5 = list,
        "select_tab_6" => kb.select_tab_6 = list,
        "select_tab_7" => kb.select_tab_7 = list,
        "select_tab_8" => kb.select_tab_8 = list,
        "select_tab_9" => kb.select_tab_9 = list,
        "copy" => kb.copy = list,
        "paste" => kb.paste = list,
        "scroll_up" => kb.scroll_up = list,
        "scroll_down" => kb.scroll_down = list,
        "scroll_top" => kb.scroll_top = list,
        "scroll_bottom" => kb.scroll_bottom = list,
        "find" => kb.find = list,
        "zoom_in" => kb.zoom_in = list,
        "zoom_out" => kb.zoom_out = list,
        "zoom_reset" => kb.zoom_reset = list,
        "command_palette" => kb.command_palette = list,
        "settings" => kb.settings = list,
        "new_window" => kb.new_window = list,
        "word_left" => kb.word_left = list,
        "word_right" => kb.word_right = list,
        "delete_word_backward" => kb.delete_word_backward = list,
        "delete_word_backward_ctrl" => kb.delete_word_backward_ctrl = list,
        "delete_word_forward" => kb.delete_word_forward = list,
        "line_start" => kb.line_start = list,
        "line_end" => kb.line_end = list,
        _ => tracing::warn!(action, "unhandled keybinding action"),
    }
}

/// Apply a `theme.<field>` color key to the config's inline theme.
fn apply_theme_color_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let hex = value.as_str().ok_or("theme color value must be a string")?;
    scribe_common::theme::hex_to_rgba(hex).map_err(|e| format!("invalid hex color: {e}"))?;

    if config.theme.is_none() {
        config.theme = Some(seed_theme_config(&config.appearance.theme));
    }

    let tc = config.theme.as_mut().ok_or("theme config unexpectedly missing")?;

    match key {
        "theme.foreground" => hex.clone_into(&mut tc.foreground),
        "theme.background" => hex.clone_into(&mut tc.background),
        "theme.cursor" => hex.clone_into(&mut tc.cursor),
        "theme.cursor_text" => hex.clone_into(&mut tc.cursor_accent),
        "theme.selection" => hex.clone_into(&mut tc.selection),
        "theme.selection_text" => hex.clone_into(&mut tc.selection_foreground),
        key if key.starts_with("theme.ansi_normal.") => {
            let idx_str = key.get("theme.ansi_normal.".len()..).ok_or("invalid ansi_normal key")?;
            let idx: usize =
                idx_str.parse().map_err(|_| String::from("invalid ansi_normal index"))?;
            if idx > 7 {
                return Err(format!("ansi_normal index {idx} out of range 0-7"));
            }
            let slot = tc
                .colors
                .get_mut(idx)
                .ok_or_else(|| format!("ansi_normal index {idx} out of range"))?;
            hex.clone_into(slot);
        }
        key if key.starts_with("theme.ansi_bright.") => {
            let idx_str = key.get("theme.ansi_bright.".len()..).ok_or("invalid ansi_bright key")?;
            let idx: usize =
                idx_str.parse().map_err(|_| String::from("invalid ansi_bright index"))?;
            if idx > 7 {
                return Err(format!("ansi_bright index {idx} out of range 0-7"));
            }
            let slot = tc
                .colors
                .get_mut(idx + 8)
                .ok_or_else(|| format!("ansi_bright index {idx} out of range"))?;
            hex.clone_into(slot);
        }
        _ => return Err(format!("unhandled theme color key: {key}")),
    }

    config.appearance.theme = String::from("custom");
    Ok(())
}

/// Build a `ThemeConfig` seeded from the named preset, converted to hex strings.
fn seed_theme_config(preset_name: &str) -> scribe_common::config::ThemeConfig {
    use scribe_common::theme::{minimal_dark, resolve_preset, rgba_to_hex};

    let theme = resolve_preset(preset_name).unwrap_or_else(minimal_dark);

    let colors = theme.ansi_colors.iter().map(|c| rgba_to_hex(*c)).collect();

    scribe_common::config::ThemeConfig {
        name: String::from("custom"),
        foreground: rgba_to_hex(theme.foreground),
        background: rgba_to_hex(theme.background),
        cursor: rgba_to_hex(theme.cursor),
        cursor_accent: rgba_to_hex(theme.cursor_accent),
        selection: rgba_to_hex(theme.selection),
        selection_foreground: rgba_to_hex(theme.selection_foreground),
        colors,
    }
}

#[cfg(test)]
mod tests {
    use super::apply_config_key;

    #[test]
    fn applies_codex_integration_toggle() {
        let mut config = scribe_common::config::ScribeConfig::default();

        apply_config_key(
            &mut config,
            "terminal.codex_code_integration",
            &serde_json::Value::Bool(false),
        )
        .expect("codex toggle should apply");

        assert!(!config.terminal.codex_code_integration);
    }
}
