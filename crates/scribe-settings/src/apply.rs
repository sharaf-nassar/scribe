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
        // -- Theme preset -----------------------------------------------------
        "theme.preset" => {
            let preset = value.as_str().ok_or("theme preset must be a string")?;
            // Convert preset name: "minimal_dark" -> "minimal-dark"
            config.appearance.theme = preset.replace('_', "-");
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
        "idle_prompt" => &mut states.idle_prompt,
        "waiting_for_input" => &mut states.waiting_for_input,
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
        "cycle_workspace" => kb.cycle_workspace = list,
        "new_tab" => kb.new_tab = list,
        "new_claude_tab" => kb.new_claude_tab = list,
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
