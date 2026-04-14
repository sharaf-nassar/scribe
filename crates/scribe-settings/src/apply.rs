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
fn apply_config_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        key if key.starts_with("appearance.") => {
            apply_appearance_key(config, key, value)?;
        }
        "theme.preset" => {
            apply_theme_preset_key(config, value)?;
        }
        key if key.starts_with("terminal.") => {
            apply_terminal_key(config, key, value)?;
        }
        key if key.starts_with("claude_states.") => {
            apply_claude_state_key(&mut config.terminal.ai_session.claude_states, key, value)?;
        }
        key if key.starts_with("keybindings.") => {
            apply_keybindings_key(&mut config.keybindings, key, value)?;
        }
        key if key.starts_with("workspaces.") => {
            apply_workspace_key(config, key, value)?;
        }
        key if key.starts_with("update.") => {
            apply_update_key(config, key, value)?;
        }
        key if key.starts_with("notifications.") => {
            apply_notifications_key(config, key, value)?;
        }
        key if key.starts_with("theme.") => {
            apply_theme_color_key(config, key, value)?;
        }
        _ => tracing::debug!(key, "unhandled settings key"),
    }

    Ok(())
}

fn parse_number<T>(value: &serde_json::Value, field: &str) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value.clone()).map_err(|_| format!("{field} must be a number"))
}

fn apply_appearance_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "appearance.font_family"
        | "appearance.font_size"
        | "appearance.font_weight"
        | "appearance.bold_weight"
        | "appearance.ligatures"
        | "appearance.line_padding"
        | "appearance.cursor_shape"
        | "appearance.cursor_blink"
        | "appearance.opacity" => apply_appearance_typography_key(config, key, value),
        "appearance.scrollbar_width"
        | "appearance.tab_bar_padding"
        | "appearance.tab_width"
        | "appearance.status_bar_height"
        | "appearance.tab_height" => apply_appearance_size_key(config, key, value),
        "appearance.content_padding_top"
        | "appearance.content_padding_right"
        | "appearance.content_padding_bottom"
        | "appearance.content_padding_left" => apply_appearance_padding_key(config, key, value),
        "appearance.focus_border_width" => apply_appearance_focus_width_key(config, value),
        "appearance.focus_border_color"
        | "appearance.prompt_bar_second_row_bg"
        | "appearance.prompt_bar_bg"
        | "appearance.prompt_bar_first_row_bg"
        | "appearance.prompt_bar_text"
        | "appearance.prompt_bar_icon_first"
        | "appearance.prompt_bar_icon_latest" => apply_appearance_color_key(config, key, value),
        _ => Err(format!("unhandled appearance key: {key}")),
    }
}

fn apply_appearance_typography_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "appearance.font_family" => {
            value
                .as_str()
                .ok_or("font_family must be a string")?
                .clone_into(&mut config.appearance.font);
        }
        "appearance.font_size" => {
            let v: f32 = parse_number(value, "font_size")?;
            config.appearance.font_size = v;
        }
        "appearance.font_weight" => {
            let v: u16 = parse_number(value, "font_weight")?;
            config.appearance.font_weight = v;
        }
        "appearance.bold_weight" => {
            let v: u16 = parse_number(value, "bold_weight")?;
            config.appearance.font_weight_bold = v;
        }
        "appearance.ligatures" => {
            config.appearance.ligatures = value.as_bool().ok_or("ligatures must be a boolean")?;
        }
        "appearance.line_padding" => {
            let v: u16 = parse_number(value, "line_padding")?;
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
            let v: f32 = parse_number(value, "opacity")?;
            config.appearance.opacity = v;
        }
        _ => return Err(format!("unhandled appearance typography key: {key}")),
    }

    Ok(())
}

fn apply_appearance_size_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "appearance.scrollbar_width" => {
            let v: f32 = parse_number(value, "scrollbar_width")?;
            config.appearance.scrollbar_width = v.clamp(2.0, 20.0);
        }
        "appearance.tab_bar_padding" => {
            let v: f32 = parse_number(value, "tab_bar_padding")?;
            config.appearance.tab_bar_padding = v.clamp(0.0, 20.0);
        }
        "appearance.tab_width" => {
            let v: u16 = parse_number(value, "tab_width")?;
            config.appearance.tab_width = v.clamp(8, 50);
        }
        "appearance.status_bar_height" => {
            let v: f32 = parse_number(value, "status_bar_height")?;
            config.appearance.status_bar_height = v.clamp(8.0, 48.0);
        }
        "appearance.tab_height" => {
            let v: f32 = parse_number(value, "tab_height")?;
            config.appearance.tab_height = v.clamp(16.0, 60.0);
        }
        _ => return Err(format!("unhandled appearance layout key: {key}")),
    }

    Ok(())
}

fn apply_appearance_padding_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "appearance.content_padding_top" => {
            let v: f32 = parse_number(value, "content_padding_top")?;
            config.appearance.content_padding.top = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_right" => {
            let v: f32 = parse_number(value, "content_padding_right")?;
            config.appearance.content_padding.right = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_bottom" => {
            let v: f32 = parse_number(value, "content_padding_bottom")?;
            config.appearance.content_padding.bottom = v.clamp(0.0, 50.0);
        }
        "appearance.content_padding_left" => {
            let v: f32 = parse_number(value, "content_padding_left")?;
            config.appearance.content_padding.left = v.clamp(0.0, 50.0);
        }
        _ => return Err(format!("unhandled appearance padding key: {key}")),
    }

    Ok(())
}

fn apply_appearance_focus_width_key(
    config: &mut scribe_common::config::ScribeConfig,
    value: &serde_json::Value,
) -> Result<(), String> {
    let v: f32 = parse_number(value, "focus_border_width")?;
    config.appearance.focus_border_width = v.clamp(1.0, 10.0);
    Ok(())
}

fn apply_appearance_color_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
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
        "appearance.prompt_bar_second_row_bg" | "appearance.prompt_bar_bg" => {
            apply_optional_hex_color(value, &mut config.appearance.prompt_bar_second_row_bg, key)?;
        }
        "appearance.prompt_bar_first_row_bg" => {
            apply_optional_hex_color(value, &mut config.appearance.prompt_bar_first_row_bg, key)?;
        }
        "appearance.prompt_bar_text" => {
            apply_optional_hex_color(value, &mut config.appearance.prompt_bar_text, key)?;
        }
        "appearance.prompt_bar_icon_first" => {
            apply_optional_hex_color(value, &mut config.appearance.prompt_bar_icon_first, key)?;
        }
        "appearance.prompt_bar_icon_latest" => {
            apply_optional_hex_color(value, &mut config.appearance.prompt_bar_icon_latest, key)?;
        }
        _ => return Err(format!("unhandled appearance color key: {key}")),
    }

    Ok(())
}

fn apply_theme_preset_key(
    config: &mut scribe_common::config::ScribeConfig,
    value: &serde_json::Value,
) -> Result<(), String> {
    let preset = value.as_str().ok_or("theme preset must be a string")?;
    // Convert preset name: "minimal_dark" -> "minimal-dark"
    config.appearance.theme = preset.replace('_', "-");
    if preset != "custom" {
        config.theme = None;
    }
    Ok(())
}

fn apply_terminal_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.scrollback_lines"
        | "terminal.copy_on_select"
        | "terminal.claude_copy_cleanup"
        | "terminal.claude_code_integration"
        | "terminal.codex_code_integration"
        | "terminal.hide_codex_hook_logs"
        | "terminal.preserve_ai_scrollback"
        | "terminal.natural_scroll" => apply_terminal_behavior_key(config, key, value),
        "terminal.prompt_bar"
        | "terminal.prompt_bar_font_size"
        | "terminal.prompt_bar_position"
        | "terminal.indicator_height" => apply_terminal_prompt_key(config, key, value),
        "terminal.status_bar_stats.cpu"
        | "terminal.status_bar_stats.memory"
        | "terminal.status_bar_stats.gpu"
        | "terminal.status_bar_stats.network" => apply_terminal_stats_key(config, key, value),
        _ => Err(format!("unhandled terminal key: {key}")),
    }
}

fn apply_terminal_behavior_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.scrollback_lines" => {
            let v: u32 = parse_number(value, "scrollback_lines")?;
            config.terminal.scrollback_lines = v;
        }
        "terminal.copy_on_select" => {
            config.terminal.clipboard.copy_on_select =
                value.as_bool().ok_or("copy_on_select must be a boolean")?;
        }
        "terminal.claude_copy_cleanup" => {
            config.terminal.clipboard.claude_copy_cleanup =
                value.as_bool().ok_or("claude_copy_cleanup must be a boolean")?;
        }
        "terminal.claude_code_integration" => {
            config.terminal.ai_integration.claude_code_integration =
                value.as_bool().ok_or("claude_code_integration must be a boolean")?;
        }
        "terminal.codex_code_integration" => {
            config.terminal.ai_integration.codex_code_integration =
                value.as_bool().ok_or("codex_code_integration must be a boolean")?;
        }
        "terminal.hide_codex_hook_logs" => {
            config.terminal.ai_session.hide_codex_hook_logs =
                value.as_bool().ok_or("hide_codex_hook_logs must be a boolean")?;
        }
        "terminal.preserve_ai_scrollback" => {
            config.terminal.ai_session.preserve_ai_scrollback =
                value.as_bool().ok_or("preserve_ai_scrollback must be a boolean")?;
        }
        "terminal.natural_scroll" => {
            config.terminal.scroll.natural_scroll =
                value.as_bool().ok_or("natural_scroll must be a boolean")?;
        }
        _ => return Err(format!("unhandled terminal key: {key}")),
    }

    Ok(())
}

fn apply_terminal_prompt_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.prompt_bar" => {
            config.terminal.prompt_bar.enabled =
                value.as_bool().ok_or("prompt_bar must be a boolean")?;
        }
        "terminal.prompt_bar_font_size" => {
            let v: f32 = parse_number(value, "prompt_bar_font_size")?;
            config.terminal.prompt_bar.font_size = v.clamp(8.0, 32.0);
        }
        "terminal.prompt_bar_position" => {
            let s = value.as_str().ok_or("prompt_bar_position must be a string")?;
            config.terminal.prompt_bar.position = match s {
                "top" => scribe_common::config::PromptBarPosition::Top,
                "bottom" => scribe_common::config::PromptBarPosition::Bottom,
                _ => return Err(format!("invalid prompt_bar_position: {s}")),
            };
        }
        "terminal.indicator_height" => {
            let v: f32 = parse_number(value, "indicator_height")?;
            config.terminal.ai_session.indicator_height = v.clamp(1.0, 10.0);
        }
        _ => return Err(format!("unhandled terminal prompt key: {key}")),
    }

    Ok(())
}

fn apply_terminal_stats_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.status_bar_stats.cpu" => {
            config.terminal.status_bar_stats.usage.compute.cpu =
                value.as_bool().ok_or("status_bar_stats.cpu must be a boolean")?;
        }
        "terminal.status_bar_stats.memory" => {
            config.terminal.status_bar_stats.usage.memory =
                value.as_bool().ok_or("status_bar_stats.memory must be a boolean")?;
        }
        "terminal.status_bar_stats.gpu" => {
            config.terminal.status_bar_stats.usage.compute.gpu =
                value.as_bool().ok_or("status_bar_stats.gpu must be a boolean")?;
        }
        "terminal.status_bar_stats.network" => {
            config.terminal.status_bar_stats.network =
                value.as_bool().ok_or("status_bar_stats.network must be a boolean")?;
        }
        _ => return Err(format!("unhandled terminal stats key: {key}")),
    }

    Ok(())
}

fn apply_keybindings_key(
    kb: &mut scribe_common::config::KeybindingsConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let action =
        key.strip_prefix("keybindings.").ok_or_else(|| format!("invalid keybinding key: {key}"))?;
    let combos = match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => {
            arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
        }
        _ => return Err(String::from("keybinding value must be a string or array")),
    };
    apply_keybinding_field(kb, action, combos);
    Ok(())
}

fn apply_workspace_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
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
        _ => return Err(format!("unhandled workspace key: {key}")),
    }

    Ok(())
}

fn apply_update_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "update.enabled" => {
            let v = value.as_bool().ok_or("update.enabled must be a boolean")?;
            config.update.enabled = v;
        }
        "update.check_interval_hours" => {
            let hours = parse_number::<u64>(value, "update.check_interval_hours")?.clamp(1, 168);
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
        _ => return Err(format!("unhandled update key: {key}")),
    }

    Ok(())
}

fn apply_notifications_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "notifications.enabled" => {
            config.notifications.enabled =
                value.as_bool().ok_or("notifications.enabled must be a boolean")?;
        }
        "notifications.condition" => {
            let s = value.as_str().ok_or("notifications.condition must be a string")?;
            config.notifications.condition = match s {
                "when_unfocused" => scribe_common::config::NotifyCondition::WhenUnfocused,
                "when_unfocused_or_background_tab" => {
                    scribe_common::config::NotifyCondition::WhenUnfocusedOrBackgroundTab
                }
                other => return Err(format!("unknown notify condition: {other}")),
            };
        }
        _ => return Err(format!("unhandled notifications key: {key}")),
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
            let v: u32 = parse_number(value, "pulse_ms")?;
            entry.pulse_ms = v;
        }
        "timeout_secs" => {
            let v: f32 = parse_number(value, "timeout_secs")?;
            entry.timeout_secs = v.max(0.0);
        }
        _ => return Err(format!("unknown claude state field: {field}")),
    }

    Ok(())
}

/// Route a keybinding action name + combo list to the correct config field.
fn apply_keybinding_field(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    combos: Vec<String>,
) {
    use scribe_common::config::KeyComboList;
    let list = KeyComboList::from_vec(combos);
    if apply_keybinding_split_and_focus_actions(kb, action, &list) {
        return;
    }
    if apply_keybinding_workspace_actions(kb, action, &list) {
        return;
    }
    if apply_keybinding_tab_actions(kb, action, &list) {
        return;
    }
    if apply_keybinding_editing_actions(kb, action, &list) {
        return;
    }
    tracing::warn!(action, "unhandled keybinding action");
}

fn apply_keybinding_split_and_focus_actions(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    list: &scribe_common::config::KeyComboList,
) -> bool {
    match action {
        "split_vertical" => kb.split_vertical = list.clone(),
        "split_horizontal" => kb.split_horizontal = list.clone(),
        "close_pane" => kb.close_pane = list.clone(),
        "cycle_pane" => kb.cycle_pane = list.clone(),
        "focus_left" => kb.focus_left = list.clone(),
        "focus_right" => kb.focus_right = list.clone(),
        "focus_up" => kb.focus_up = list.clone(),
        "focus_down" => kb.focus_down = list.clone(),
        "workspace_split_vertical" => kb.workspace_split_vertical = list.clone(),
        "workspace_split_horizontal" => kb.workspace_split_horizontal = list.clone(),
        _ => return false,
    }
    true
}

fn apply_keybinding_workspace_actions(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    list: &scribe_common::config::KeyComboList,
) -> bool {
    match action {
        "workspace_focus_left" => kb.workspace_focus_left = list.clone(),
        "workspace_focus_right" => kb.workspace_focus_right = list.clone(),
        "workspace_focus_up" => kb.workspace_focus_up = list.clone(),
        "workspace_focus_down" => kb.workspace_focus_down = list.clone(),
        _ => return false,
    }
    true
}

fn apply_keybinding_tab_actions(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    list: &scribe_common::config::KeyComboList,
) -> bool {
    match action {
        "new_tab" => kb.new_tab = list.clone(),
        "new_claude_tab" => kb.new_claude_tab = list.clone(),
        "new_claude_resume_tab" => kb.new_claude_resume_tab = list.clone(),
        "new_codex_tab" => kb.new_codex_tab = list.clone(),
        "new_codex_resume_tab" => kb.new_codex_resume_tab = list.clone(),
        "close_tab" => kb.close_tab = list.clone(),
        "next_tab" => kb.next_tab = list.clone(),
        "prev_tab" => kb.prev_tab = list.clone(),
        "select_tab_1" => kb.select_tab_1 = list.clone(),
        "select_tab_2" => kb.select_tab_2 = list.clone(),
        "select_tab_3" => kb.select_tab_3 = list.clone(),
        "select_tab_4" => kb.select_tab_4 = list.clone(),
        "select_tab_5" => kb.select_tab_5 = list.clone(),
        "select_tab_6" => kb.select_tab_6 = list.clone(),
        "select_tab_7" => kb.select_tab_7 = list.clone(),
        "select_tab_8" => kb.select_tab_8 = list.clone(),
        "select_tab_9" => kb.select_tab_9 = list.clone(),
        _ => return false,
    }
    true
}

fn apply_keybinding_editing_actions(
    kb: &mut scribe_common::config::KeybindingsConfig,
    action: &str,
    list: &scribe_common::config::KeyComboList,
) -> bool {
    match action {
        "copy" => kb.copy = list.clone(),
        "paste" => kb.paste = list.clone(),
        "scroll_up" => kb.scroll_up = list.clone(),
        "scroll_down" => kb.scroll_down = list.clone(),
        "scroll_top" => kb.scroll_top = list.clone(),
        "scroll_bottom" => kb.scroll_bottom = list.clone(),
        "find" => kb.find = list.clone(),
        "zoom_in" => kb.zoom_in = list.clone(),
        "zoom_out" => kb.zoom_out = list.clone(),
        "zoom_reset" => kb.zoom_reset = list.clone(),
        "command_palette" => kb.command_palette = list.clone(),
        "settings" => kb.settings = list.clone(),
        "new_window" => kb.new_window = list.clone(),
        "word_left" => kb.word_left = list.clone(),
        "word_right" => kb.word_right = list.clone(),
        "delete_word_backward" => kb.delete_word_backward = list.clone(),
        "delete_word_backward_ctrl" => kb.delete_word_backward_ctrl = list.clone(),
        "delete_word_forward" => kb.delete_word_forward = list.clone(),
        "line_start" => kb.line_start = list.clone(),
        "line_end" => kb.line_end = list.clone(),
        _ => return false,
    }
    true
}

/// Apply an optional hex color override: empty string clears it to `None`.
fn apply_optional_hex_color(
    value: &serde_json::Value,
    field: &mut Option<String>,
    key: &str,
) -> Result<(), String> {
    let hex = value.as_str().ok_or(format!("{key} must be a string"))?;
    if hex.is_empty() {
        *field = None;
    } else {
        scribe_common::theme::hex_to_rgba(hex).map_err(|e| format!("invalid {key}: {e}"))?;
        *field = Some(hex.to_owned());
    }
    Ok(())
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

        assert!(!config.terminal.ai_integration.codex_code_integration);
    }
}
