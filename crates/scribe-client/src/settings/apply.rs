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
pub(crate) fn apply_config_key(
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
        key if key.starts_with("ai_states.") || key.starts_with("claude_states.") => {
            apply_ai_state_key(&mut config.terminal.ai_session.ai_states, key, value)?;
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
        key if key.starts_with("remote.") => {
            apply_remote_key(config, key, value)?;
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
            apply_optional_hex_color(value, &mut config.appearance.focus_border_color, key)?;
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
        | "terminal.preserve_ai_scrollback"
        | "terminal.natural_scroll"
        | "terminal.keyboard_protocol_enhanced"
        | "terminal.paste_confirmation"
        | "terminal.images.enabled"
        | "terminal.env_persistence.enabled" => apply_terminal_behavior_key(config, key, value),
        "terminal.prompt_bar"
        | "terminal.prompt_bar_font_size"
        | "terminal.prompt_bar_position"
        | "terminal.indicator_height" => apply_terminal_prompt_key(config, key, value),
        "terminal.smart_selection" | "terminal.smart_selection.reset" => {
            apply_terminal_smart_selection_key(config, key, value)
        }
        "terminal.status_bar_stats.cpu"
        | "terminal.status_bar_stats.memory"
        | "terminal.status_bar_stats.gpu"
        | "terminal.status_bar_stats.network" => apply_terminal_stats_key(config, key, value),
        "terminal.clipboard.read_mode"
        | "terminal.clipboard.write_mode"
        | "terminal.clipboard.max_write_bytes"
        | "terminal.clipboard.focus_gate_writes" => {
            apply_terminal_clipboard_key(config, key, value)
        }
        _ => Err(format!("unhandled terminal key: {key}")),
    }
}

/// Spec 010 T035 / T047: apply OSC 52 clipboard policy keys from the
/// settings webview. The Rust field on `TerminalConfig` is `clipboard_policy`
/// but the serde-renamed TOML namespace is `terminal.clipboard.*`, so this
/// handler stores into `config.terminal.clipboard_policy.{read_mode,
/// write_mode, max_write_bytes, focus_gate_writes}`. `max_write_bytes` is
/// clamped here to the public ceiling from
/// [`scribe_common::config::CLIPBOARD_MAX_WRITE_BYTES_CEILING`] (512 MiB) to
/// match the deserialize-time clamp, so the on-disk config stays in range
/// even if the webview ever sends an out-of-band value. The
/// `focus_gate_writes` toggle (FR-019) is a plain bool that the client
/// consults at bridge-write time; the server never inspects it.
fn apply_terminal_clipboard_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.clipboard.read_mode" => {
            let s = value.as_str().ok_or("clipboard.read_mode must be a string")?;
            config.terminal.clipboard_policy.read_mode = parse_clipboard_mode(s)?;
        }
        "terminal.clipboard.write_mode" => {
            let s = value.as_str().ok_or("clipboard.write_mode must be a string")?;
            config.terminal.clipboard_policy.write_mode = parse_clipboard_mode(s)?;
        }
        "terminal.clipboard.max_write_bytes" => {
            let v: u64 = parse_number(value, "clipboard.max_write_bytes")?;
            config.terminal.clipboard_policy.max_write_bytes =
                v.min(scribe_common::config::CLIPBOARD_MAX_WRITE_BYTES_CEILING);
        }
        "terminal.clipboard.focus_gate_writes" => {
            config.terminal.clipboard_policy.focus_gate_writes =
                value.as_bool().ok_or("clipboard.focus_gate_writes must be a boolean")?;
        }
        _ => return Err(format!("unhandled terminal clipboard key: {key}")),
    }

    Ok(())
}

fn parse_clipboard_mode(s: &str) -> Result<scribe_common::config::ClipboardMode, String> {
    match s {
        "deny" => Ok(scribe_common::config::ClipboardMode::Deny),
        "allow" => Ok(scribe_common::config::ClipboardMode::Allow),
        "prompt" => Ok(scribe_common::config::ClipboardMode::Prompt),
        _ => Err(format!("invalid clipboard mode: {s}")),
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
            config.terminal.ai_integration.claude_code =
                scribe_common::config::AiIntegrationToggle::new(
                    value.as_bool().ok_or("claude_code_integration must be a boolean")?,
                );
        }
        "terminal.codex_code_integration" => {
            config.terminal.ai_integration.codex_code =
                scribe_common::config::AiIntegrationToggle::new(
                    value.as_bool().ok_or("codex_code_integration must be a boolean")?,
                );
        }
        "terminal.preserve_ai_scrollback" => {
            config.terminal.ai_session.preserve_ai_scrollback =
                value.as_bool().ok_or("preserve_ai_scrollback must be a boolean")?;
        }
        "terminal.natural_scroll" => {
            config.terminal.scroll.natural_scroll =
                value.as_bool().ok_or("natural_scroll must be a boolean")?;
        }
        "terminal.keyboard_protocol_enhanced" => {
            config.terminal.keyboard_protocol_enhanced =
                value.as_bool().ok_or("keyboard_protocol_enhanced must be a boolean")?;
        }
        "terminal.paste_confirmation" => {
            config.terminal.paste_confirmation =
                value.as_bool().ok_or("paste_confirmation must be a boolean")?;
        }
        // Spec 020: the terminal-image master switch. The server applies it
        // live on `ConfigReloaded` — disabling stops advertising and releases
        // image state; re-enabling waits for a capable viewer to latch again.
        "terminal.images.enabled" => {
            config.terminal.images.enabled =
                value.as_bool().ok_or("images.enabled must be a boolean")?;
        }
        "terminal.env_persistence.enabled" => {
            config.terminal.env_persistence.enabled =
                value.as_bool().ok_or("env_persistence.enabled must be a boolean")?;
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
            // Moving the stepper pins an explicit size; the strip stops
            // following `appearance.font_size` until the key is deleted.
            config.terminal.prompt_bar.font_size = Some(v.clamp(8.0, 32.0));
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

fn apply_terminal_smart_selection_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "terminal.smart_selection" => {
            let smart_selection: scribe_common::config::SmartSelectionConfig =
                serde_json::from_value(value.clone())
                    .map_err(|e| format!("invalid smart_selection payload: {e}"))?;
            validate_smart_selection_config(&smart_selection)?;
            config.terminal.smart_selection = smart_selection;
        }
        "terminal.smart_selection.reset" => {
            config.terminal.smart_selection =
                scribe_common::config::SmartSelectionConfig::default();
        }
        _ => return Err(format!("unhandled terminal smart selection key: {key}")),
    }

    Ok(())
}

fn validate_smart_selection_config(
    config: &scribe_common::config::SmartSelectionConfig,
) -> Result<(), String> {
    for (idx, rule) in config.rules.iter().enumerate() {
        if !rule.enabled {
            continue;
        }
        if rule.regex.trim().is_empty() {
            return Err(format!("smart selection rule {} has an empty regex", idx + 1));
        }
        regex::Regex::new(&rule.regex)
            .map_err(|e| format!("smart selection rule {} regex is invalid: {e}", idx + 1))?;
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
            let root = workspace_root_from_value(value)?;
            if !config.workspaces.roots.iter().any(|existing| existing == &root) {
                config.workspaces.roots.push(root);
            }
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
            let index_str =
                key.strip_prefix("workspaces.badge_colors.").ok_or("invalid badge color key")?;
            let index: usize =
                index_str.parse().map_err(|_| String::from("invalid badge color index"))?;
            let color = canonical_color_value(key, value)?;
            let slot = config
                .workspaces
                .badge_colors
                .get_mut(index)
                .ok_or_else(|| format!("badge color index {index} is out of range"))?;
            color.clone_into(slot);
        }
        _ => return Err(format!("unhandled workspace key: {key}")),
    }

    Ok(())
}

pub(crate) fn workspace_root_from_value(value: &serde_json::Value) -> Result<String, String> {
    let root = value.as_str().ok_or("add_root value must be a string")?.trim();
    if root.is_empty() {
        return Err(String::from("Workspace root must not be empty"));
    }
    if !root.starts_with("~/") && !std::path::Path::new(root).is_absolute() {
        return Err(String::from("Workspace root must be absolute or start with ~/"));
    }
    Ok(root.to_owned())
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
                "always" => scribe_common::config::NotifyCondition::Always,
                "when_unfocused_or_background_tab" => {
                    scribe_common::config::NotifyCondition::WhenUnfocusedOrBackgroundTab
                }
                other => return Err(format!("unknown notify condition: {other}")),
            };
        }
        "notifications.timeout_mode" => {
            let s = value.as_str().ok_or("notifications.timeout_mode must be a string")?;
            config.notifications.timeout_mode = match s {
                "system_default" => scribe_common::config::NotifyTimeoutMode::SystemDefault,
                "custom" => scribe_common::config::NotifyTimeoutMode::Custom,
                "never" => scribe_common::config::NotifyTimeoutMode::Never,
                other => return Err(format!("unknown notify timeout mode: {other}")),
            };
        }
        "notifications.timeout_secs" => {
            config.notifications.timeout_secs = parse_number(value, "notifications.timeout_secs")?;
        }
        _ => return Err(format!("unhandled notifications key: {key}")),
    }

    Ok(())
}

/// Apply a `remote.<field>` settings change to the `[remote]` TOML table.
///
/// Feature 013 (tailnet): `remote.enabled` toggles the opt-in Tailscale
/// remote-control listener (default off); `remote.port` sets the TCP port bound
/// only on the machine's tailnet addresses.
///
/// Feature 014 (LAN): `remote.lan.enabled` toggles the separate opt-in LAN
/// listener (default off; a distinct opt-in from the tailnet listener, FR-012),
/// and `remote.lan.port` sets the port bound only on the physical LAN address
/// (default 46062). Both ports are clamped to the same 1024–65535 range the
/// settings webview enforces so a hand-crafted IPC cannot persist an
/// out-of-range value. The server applies all four live on `ConfigReloaded`; it
/// is never restarted for this.
///
/// Feature 015 (window sharing): `remote.sharing_mode` selects who may type into
/// a shared window (`single_controller` default / `shared_single_typist` /
/// `free_for_all`, FR-004); `remote.control_acquisition` selects how control is
/// handed off in single-typist mode (`free_claim` default / `request_and_grant`,
/// FR-005); `remote.participant_limit` caps remote joins per shared window, with
/// `0` persisted as `None` (unlimited, FR-018). The server reconciles live
/// shares on `ConfigReloaded`; no restart.
fn apply_remote_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        "remote.enabled" => {
            config.remote.enabled = value.as_bool().ok_or("remote.enabled must be a boolean")?;
        }
        "remote.port" => {
            let v: u16 = parse_number(value, "remote.port")?;
            config.remote.port = v.clamp(1024, 65535);
        }
        "remote.lan.enabled" => {
            config.remote.lan.enabled =
                value.as_bool().ok_or("remote.lan.enabled must be a boolean")?;
        }
        "remote.lan.port" => {
            let v: u16 = parse_number(value, "remote.lan.port")?;
            config.remote.lan.port = v.clamp(1024, 65535);
        }
        "remote.sharing_mode" => {
            let s = value.as_str().ok_or("remote.sharing_mode must be a string")?;
            config.remote.sharing_mode =
                serde_json::from_value(serde_json::Value::String(s.to_owned()))
                    .map_err(|e| format!("invalid sharing mode: {e}"))?;
        }
        "remote.control_acquisition" => {
            let s = value.as_str().ok_or("remote.control_acquisition must be a string")?;
            config.remote.control_acquisition =
                serde_json::from_value(serde_json::Value::String(s.to_owned()))
                    .map_err(|e| format!("invalid control acquisition: {e}"))?;
        }
        "remote.participant_limit" => {
            let v: u32 = parse_number(value, "remote.participant_limit")?;
            config.remote.participant_limit = if v == 0 { None } else { Some(v) };
        }
        _ => return Err(format!("unhandled remote key: {key}")),
    }

    Ok(())
}

/// Apply an `ai_states.<state>.<field>` settings change.
fn apply_ai_state_key(
    states: &mut scribe_common::config::AiStateStylesConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    // Key format: "ai_states.<state>.<field>". The legacy
    // "claude_states." prefix remains accepted for old settings surfaces.
    let rest = key
        .strip_prefix("ai_states.")
        .or_else(|| key.strip_prefix("claude_states."))
        .ok_or_else(|| format!("invalid AI state key: {key}"))?;
    let (state_name, field) =
        rest.split_once('.').ok_or_else(|| format!("invalid AI state key: {key}"))?;

    let entry = match state_name {
        "processing" => &mut states.processing,
        "idle_prompt" | "waiting_for_input" => &mut states.waiting_for_input,
        "permission_prompt" => &mut states.permission_prompt,
        "error" => &mut states.error,
        _ => return Err(format!("unknown AI state: {state_name}")),
    };

    match field {
        "tab_indicator" => {
            entry.tab_indicator = value.as_bool().ok_or("tab_indicator must be a boolean")?;
        }
        "pane_border" => {
            entry.pane_border = value.as_bool().ok_or("pane_border must be a boolean")?;
        }
        "color" => {
            let color = canonical_color_value(key, value)?;
            entry.color = serde_json::from_value(serde_json::Value::String(color))
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
        "equalize" => kb.equalize = list.clone(),
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
        "jump_to_failure" => kb.jump_to_failure = list.clone(),
        "prompt_jump_up" => kb.prompt_jump_up = list.clone(),
        "prompt_jump_down" => kb.prompt_jump_down = list.clone(),
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
    let color = canonical_color_value(key, value)?;
    if color.is_empty() {
        *field = None;
    } else {
        *field = Some(color);
    }
    Ok(())
}

/// Validate and canonicalize every editable color family before config mutation.
pub(crate) fn canonical_color_value(
    key: &str,
    value: &serde_json::Value,
) -> Result<String, String> {
    let raw = value.as_str().ok_or_else(|| format!("{key} must be a string"))?;
    if key.starts_with("appearance.") && raw.is_empty() {
        return Ok(String::new());
    }
    if (key.starts_with("ai_states.") || key.starts_with("claude_states."))
        && key.strip_suffix(".color").is_some()
    {
        let color: scribe_common::config::AiColor =
            serde_json::from_value(serde_json::Value::String(raw.to_owned()))
                .map_err(|_| format!("{key} must be #rrggbb or ansi:0–15"))?;
        return serde_json::to_value(color)
            .ok()
            .and_then(|stored| stored.as_str().map(str::to_owned))
            .ok_or_else(|| format!("failed to canonicalize {key}"));
    }
    if !key.starts_with("theme.")
        && !key.starts_with("appearance.")
        && !key.starts_with("workspaces.badge_colors.")
    {
        return Err(format!("unsupported color key: {key}"));
    }
    let rgba = scribe_common::theme::hex_to_rgba(raw)
        .map_err(|_| format!("{key} must be six hex digits, with an optional #"))?;
    Ok(scribe_common::theme::rgba_to_hex(rgba))
}

/// Apply a `theme.<field>` color key to the config's inline theme.
fn apply_theme_color_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let color = canonical_color_value(key, value)?;

    let mut theme =
        config.theme.clone().unwrap_or_else(|| seed_theme_config(&config.appearance.theme));
    let tc = &mut theme;

    match key {
        "theme.foreground" => color.clone_into(&mut tc.foreground),
        "theme.background" => color.clone_into(&mut tc.background),
        "theme.cursor" => color.clone_into(&mut tc.cursor),
        "theme.cursor_text" => color.clone_into(&mut tc.cursor_accent),
        "theme.selection" => color.clone_into(&mut tc.selection),
        "theme.selection_text" => color.clone_into(&mut tc.selection_foreground),
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
            color.clone_into(slot);
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
            color.clone_into(slot);
        }
        _ => return Err(format!("unhandled theme color key: {key}")),
    }

    config.theme = Some(theme);
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
    use super::{apply_config_key, canonical_color_value, workspace_root_from_value};

    #[test]
    fn applies_codex_integration_toggle() {
        let mut config = scribe_common::config::ScribeConfig::default();

        apply_config_key(
            &mut config,
            "terminal.codex_code_integration",
            &serde_json::Value::Bool(false),
        )
        .expect("codex toggle should apply");

        assert!(!config.terminal.ai_integration.codex_code.enabled());
    }

    #[test]
    fn adds_workspace_root_path() {
        let mut config = scribe_common::config::ScribeConfig::default();

        apply_config_key(
            &mut config,
            "workspaces.add_root",
            &serde_json::Value::String(String::from("/home/user/work")),
        )
        .expect("workspace root should apply");

        assert_eq!(config.workspaces.roots, vec![String::from("/home/user/work")]);
    }

    #[test]
    fn validates_workspace_root_paths() {
        assert_eq!(
            workspace_root_from_value(&serde_json::Value::String(String::from("  /srv/work  "))),
            Ok(String::from("/srv/work"))
        );
        assert_eq!(
            workspace_root_from_value(&serde_json::Value::String(String::from("~/work"))),
            Ok(String::from("~/work"))
        );
        assert_eq!(
            workspace_root_from_value(&serde_json::Value::String(String::from("  "))),
            Err(String::from("Workspace root must not be empty"))
        );
        for invalid in ["~", "relative/path"] {
            assert_eq!(
                workspace_root_from_value(&serde_json::Value::String(invalid.to_owned())),
                Err(String::from("Workspace root must be absolute or start with ~/"))
            );
        }
    }

    #[test]
    fn validates_and_canonicalizes_color_families() {
        assert_eq!(
            canonical_color_value("workspaces.badge_colors.0", &serde_json::json!("A1b2C3")),
            Ok("#a1b2c3".to_owned())
        );
        assert_eq!(
            canonical_color_value("appearance.focus_border_color", &serde_json::json!("")),
            Ok(String::new())
        );
        assert_eq!(
            canonical_color_value("ai_states.processing.color", &serde_json::json!("ansi:03")),
            Ok("ansi:3".to_owned())
        );
        assert!(
            canonical_color_value("workspaces.badge_colors.0", &serde_json::json!("#12345"))
                .is_err()
        );
    }

    #[test]
    fn rejects_missing_workspace_color_without_mutating_config() {
        let mut config = scribe_common::config::ScribeConfig::default();
        let before = config.workspaces.badge_colors.clone();

        assert!(
            apply_config_key(
                &mut config,
                "workspaces.badge_colors.999",
                &serde_json::json!("#112233"),
            )
            .is_err()
        );
        assert_eq!(config.workspaces.badge_colors, before);
    }

    #[test]
    fn theme_color_edit_selects_custom_theme() {
        let mut config = scribe_common::config::ScribeConfig::default();

        apply_config_key(&mut config, "theme.foreground", &serde_json::json!("ABCDEF"))
            .expect("theme color should apply");

        assert_eq!(config.appearance.theme, "custom");
        assert_eq!(config.theme.expect("inline theme").foreground, "#abcdef");
    }
}
