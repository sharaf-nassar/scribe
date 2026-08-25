//! Pure state operations for the GPUI Smart Selection settings editor.

use regex::Regex;
use scribe_common::config::{
    SmartSelectionAction, SmartSelectionActionKind, SmartSelectionActivation, SmartSelectionConfig,
    SmartSelectionParameterMode, SmartSelectionPrecision, SmartSelectionRule,
};
use serde_json::Value;

pub(super) const ACTIVATION_OPTIONS: &[(&str, &str)] =
    &[("double_click", "Double click"), ("quad_click", "Quad click")];
pub(super) const PRECISION_OPTIONS: &[(&str, &str)] = &[
    ("very_low", "Very low"),
    ("low", "Low"),
    ("normal", "Normal"),
    ("high", "High"),
    ("very_high", "Very high"),
];
pub(super) const ACTION_KIND_OPTIONS: &[(&str, &str)] = &[
    ("open_file", "Open file"),
    ("open_url", "Open URL"),
    ("run_command", "Run command"),
    ("run_coprocess", "Run coprocess"),
    ("send_text", "Send text"),
    ("run_command_in_window", "Run command in window"),
    ("copy", "Copy"),
];
pub(super) const PARAMETER_MODE_OPTIONS: &[(&str, &str)] =
    &[("legacy", "Legacy"), ("interpolated", "Interpolated")];

const PREFIX: &str = "terminal.smart_selection.";
const ACTIVATION_KEY: &str = "terminal.smart_selection.activation";
pub(super) const PREVIEW_KEY: &str = "terminal.smart_selection.preview";
pub(super) const PREVIEW_CURSOR_KEY: &str = "terminal.smart_selection.preview_cursor";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SmartActionTarget {
    SelectRule(usize),
    AddRule,
    DuplicateRule,
    RemoveRule,
    MoveRuleUp,
    MoveRuleDown,
    RestoreDefaults,
    AddAction,
    DuplicateAction(usize),
    RemoveAction(usize),
    MoveActionUp(usize),
    MoveActionDown(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmartControlKey {
    Activation,
    RuleEnabled(usize),
    RuleName(usize),
    RuleRegex(usize),
    RulePrecision(usize),
    ActionKind { rule: usize, action: usize },
    ActionParameter { rule: usize, action: usize },
    ActionMode { rule: usize, action: usize },
    Preview,
    PreviewCursor,
}

pub(super) fn activation_key() -> String {
    ACTIVATION_KEY.to_owned()
}

pub(super) fn rule_enabled_key(rule: usize) -> String {
    format!("{PREFIX}rules.{rule}.enabled")
}

pub(super) fn rule_name_key(rule: usize) -> String {
    format!("{PREFIX}rules.{rule}.name")
}

pub(super) fn rule_regex_key(rule: usize) -> String {
    format!("{PREFIX}rules.{rule}.regex")
}

pub(super) fn rule_precision_key(rule: usize) -> String {
    format!("{PREFIX}rules.{rule}.precision")
}

pub(super) fn action_kind_key(rule: usize, action: usize) -> String {
    format!("{PREFIX}rules.{rule}.actions.{action}.kind")
}

pub(super) fn action_parameter_key(rule: usize, action: usize) -> String {
    format!("{PREFIX}rules.{rule}.actions.{action}.parameter")
}

pub(super) fn action_mode_key(rule: usize, action: usize) -> String {
    format!("{PREFIX}rules.{rule}.actions.{action}.parameter_mode")
}

pub(super) fn is_smart_control_key(key: &str) -> bool {
    parse_key(key).is_some()
}

pub(super) fn inline_placeholder(key: &str) -> Option<&'static str> {
    match parse_key(key) {
        Some(SmartControlKey::RuleName(_)) => Some("Rule name"),
        Some(SmartControlKey::RuleRegex(_)) => Some("Rust regular expression"),
        Some(SmartControlKey::ActionParameter { .. }) => Some("Optional action parameter"),
        Some(SmartControlKey::Preview) => Some("Paste sample terminal text"),
        _ => None,
    }
}

pub(super) fn control_value(
    config: &SmartSelectionConfig,
    preview: &str,
    preview_cursor: usize,
    key: &str,
) -> Option<Value> {
    let value = match parse_key(key)? {
        SmartControlKey::Activation => {
            Value::String(activation_token(config.activation).to_owned())
        }
        SmartControlKey::RuleEnabled(index) => Value::Bool(config.rules.get(index)?.enabled),
        SmartControlKey::RuleName(index) => Value::String(config.rules.get(index)?.name.clone()),
        SmartControlKey::RuleRegex(index) => Value::String(config.rules.get(index)?.regex.clone()),
        SmartControlKey::RulePrecision(index) => {
            Value::String(precision_token(config.rules.get(index)?.precision).to_owned())
        }
        SmartControlKey::ActionKind { rule, action } => Value::String(
            action_kind_token(config.rules.get(rule)?.actions.get(action)?.kind).to_owned(),
        ),
        SmartControlKey::ActionParameter { rule, action } => {
            Value::String(config.rules.get(rule)?.actions.get(action)?.parameter.clone())
        }
        SmartControlKey::ActionMode { rule, action } => Value::String(
            parameter_mode_token(config.rules.get(rule)?.actions.get(action)?.parameter_mode)
                .to_owned(),
        ),
        SmartControlKey::Preview => Value::String(preview.to_owned()),
        SmartControlKey::PreviewCursor => serde_json::json!(preview_cursor),
    };
    Some(value)
}

/// Apply one editor control. Returns `true` when durable config changed and
/// `false` for the local-only preview field.
pub(super) fn apply_control_value(
    config: &mut SmartSelectionConfig,
    preview: &mut String,
    preview_cursor: &mut usize,
    key: &str,
    value: &Value,
) -> Result<bool, String> {
    match parse_key(key).ok_or_else(|| format!("unknown Smart Selection control: {key}"))? {
        SmartControlKey::Activation => {
            config.activation = parse_activation(value)?;
        }
        SmartControlKey::RuleEnabled(index) => {
            config.rules.get_mut(index).ok_or("rule no longer exists")?.enabled =
                value.as_bool().ok_or("enabled must be a boolean")?;
        }
        SmartControlKey::RuleName(index) => {
            let rule = config.rules.get_mut(index).ok_or("rule no longer exists")?;
            value.as_str().ok_or("name must be text")?.trim().clone_into(&mut rule.name);
        }
        SmartControlKey::RuleRegex(index) => {
            let rule = config.rules.get_mut(index).ok_or("rule no longer exists")?;
            value.as_str().ok_or("regex must be text")?.clone_into(&mut rule.regex);
        }
        SmartControlKey::RulePrecision(index) => {
            config.rules.get_mut(index).ok_or("rule no longer exists")?.precision =
                parse_precision(value)?;
        }
        SmartControlKey::ActionKind { rule, action } => {
            config
                .rules
                .get_mut(rule)
                .and_then(|rule| rule.actions.get_mut(action))
                .ok_or("action no longer exists")?
                .kind = parse_action_kind(value)?;
        }
        SmartControlKey::ActionParameter { rule, action } => {
            let action = config
                .rules
                .get_mut(rule)
                .and_then(|rule| rule.actions.get_mut(action))
                .ok_or("action no longer exists")?;
            value.as_str().ok_or("parameter must be text")?.clone_into(&mut action.parameter);
        }
        SmartControlKey::ActionMode { rule, action } => {
            config
                .rules
                .get_mut(rule)
                .and_then(|rule| rule.actions.get_mut(action))
                .ok_or("action no longer exists")?
                .parameter_mode = parse_parameter_mode(value)?;
        }
        SmartControlKey::Preview => {
            value.as_str().ok_or("preview must be text")?.clone_into(preview);
            *preview_cursor = (*preview_cursor).min(preview.chars().count().saturating_sub(1));
            return Ok(false);
        }
        SmartControlKey::PreviewCursor => {
            *preview_cursor = value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or("preview cursor must be a non-negative integer")?
                .min(preview.chars().count().saturating_sub(1));
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn validate_inline_value(
    config: &SmartSelectionConfig,
    key: &str,
    value: &str,
) -> Result<(), String> {
    match parse_key(key) {
        Some(SmartControlKey::RuleName(_)) if value.trim().is_empty() => {
            Err("Give this rule a name.".to_owned())
        }
        Some(SmartControlKey::RuleRegex(index))
            if config.rules.get(index).is_some_and(|rule| rule.enabled) =>
        {
            if value.trim().is_empty() {
                return Err("Enter a regular expression or disable the rule.".to_owned());
            }
            Regex::new(value).map(|_| ()).map_err(|error| format!("Invalid regex: {error}"))
        }
        _ => Ok(()),
    }
}

pub(super) fn rule_validation_error(rule: &SmartSelectionRule) -> Option<String> {
    if !rule.enabled {
        return None;
    }
    if rule.regex.trim().is_empty() {
        return Some("Regex is required".to_owned());
    }
    Regex::new(&rule.regex).err().map(|error| format!("Invalid regex: {error}"))
}

pub(super) fn preview_match(
    regex: &str,
    sample: &str,
    cursor: usize,
) -> Result<Option<String>, String> {
    if regex.trim().is_empty() || sample.is_empty() {
        return Ok(None);
    }
    let regex = Regex::new(regex).map_err(|error| format!("Invalid regex: {error}"))?;
    let start = sample
        .char_indices()
        .nth(cursor.min(sample.chars().count().saturating_sub(1)))
        .map_or(0, |(index, _)| index);
    let end =
        sample[start..].chars().next().map_or(start, |character| start + character.len_utf8());
    Ok(regex
        .find_iter(sample)
        .filter(|found| found.start() <= start && found.end() >= end && !found.is_empty())
        .max_by_key(regex::Match::len)
        .map(|found| found.as_str().to_owned()))
}

pub(super) fn selected_rule_index(selected: usize, len: usize) -> Option<usize> {
    (len > 0).then_some(selected.min(len - 1))
}

pub(super) fn apply_action(
    config: &mut SmartSelectionConfig,
    selected: usize,
    action: SmartActionTarget,
) -> usize {
    let Some(index) = selected_rule_index(selected, config.rules.len()) else {
        match action {
            SmartActionTarget::AddRule => config.rules.push(new_rule(&config.rules)),
            SmartActionTarget::RestoreDefaults => *config = SmartSelectionConfig::default(),
            _ => {}
        }
        return 0;
    };
    match action {
        SmartActionTarget::SelectRule(next) => next.min(config.rules.len() - 1),
        SmartActionTarget::AddRule => add_rule(config),
        SmartActionTarget::DuplicateRule => duplicate_rule(config, index),
        SmartActionTarget::RemoveRule => remove_rule(config, index),
        SmartActionTarget::MoveRuleUp if index > 0 => {
            config.rules.swap(index, index - 1);
            index - 1
        }
        SmartActionTarget::MoveRuleDown if index + 1 < config.rules.len() => {
            config.rules.swap(index, index + 1);
            index + 1
        }
        SmartActionTarget::RestoreDefaults => {
            *config = SmartSelectionConfig::default();
            0
        }
        SmartActionTarget::AddAction => {
            if let Some(rule) = config.rules.get_mut(index) {
                rule.actions.push(SmartSelectionAction::default());
            }
            index
        }
        SmartActionTarget::DuplicateAction(action_index) => {
            duplicate_action(config.rules.get_mut(index), action_index);
            index
        }
        SmartActionTarget::RemoveAction(action_index) => {
            remove_action(config.rules.get_mut(index), action_index);
            index
        }
        SmartActionTarget::MoveActionUp(action_index) if action_index > 0 => {
            move_action(config.rules.get_mut(index), action_index, action_index - 1);
            index
        }
        SmartActionTarget::MoveActionDown(action_index) => {
            move_action(config.rules.get_mut(index), action_index, action_index.saturating_add(1));
            index
        }
        SmartActionTarget::MoveRuleUp
        | SmartActionTarget::MoveRuleDown
        | SmartActionTarget::MoveActionUp(_) => index,
    }
}

fn add_rule(config: &mut SmartSelectionConfig) -> usize {
    config.rules.push(new_rule(&config.rules));
    config.rules.len() - 1
}

fn duplicate_rule(config: &mut SmartSelectionConfig, index: usize) -> usize {
    let Some(mut copy) = config.rules.get(index).cloned() else { return index };
    copy.id = next_rule_id(&config.rules);
    copy.name = format!("{} copy", copy.name.trim());
    config.rules.insert(index + 1, copy);
    index + 1
}

fn remove_rule(config: &mut SmartSelectionConfig, index: usize) -> usize {
    if index < config.rules.len() {
        config.rules.remove(index);
    }
    index.min(config.rules.len().saturating_sub(1))
}

fn duplicate_action(rule: Option<&mut SmartSelectionRule>, index: usize) {
    let Some(rule) = rule else { return };
    let Some(copy) = rule.actions.get(index).cloned() else { return };
    rule.actions.insert(index + 1, copy);
}

fn remove_action(rule: Option<&mut SmartSelectionRule>, index: usize) {
    let Some(rule) = rule else { return };
    if index < rule.actions.len() {
        rule.actions.remove(index);
    }
}

fn move_action(rule: Option<&mut SmartSelectionRule>, from: usize, to: usize) {
    let Some(rule) = rule else { return };
    if from < rule.actions.len() && to < rule.actions.len() {
        rule.actions.swap(from, to);
    }
}

pub(super) fn matches_query(config: &SmartSelectionConfig, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    if "smart selection activation gesture rules regex precision actions test preview"
        .contains(&query)
    {
        return true;
    }
    config.rules.iter().any(|rule| {
        rule.name.to_lowercase().contains(&query)
            || rule.regex.to_lowercase().contains(&query)
            || precision_label(rule.precision).to_lowercase().contains(&query)
            || rule.actions.iter().any(|action| {
                action_kind_label(action.kind).to_lowercase().contains(&query)
                    || action.parameter.to_lowercase().contains(&query)
            })
    })
}

pub(super) fn precision_label(precision: SmartSelectionPrecision) -> &'static str {
    option_label(PRECISION_OPTIONS, precision_token(precision))
}

pub(super) fn action_kind_label(kind: SmartSelectionActionKind) -> &'static str {
    option_label(ACTION_KIND_OPTIONS, action_kind_token(kind))
}

pub(super) fn action_parameter_hint(kind: SmartSelectionActionKind) -> &'static str {
    match kind {
        SmartSelectionActionKind::OpenFile => "Empty uses the match as a path.",
        SmartSelectionActionKind::OpenUrl => "Empty uses the match as a URL.",
        SmartSelectionActionKind::RunCommand => "Command sent to the focused terminal.",
        SmartSelectionActionKind::RunCoprocess => "Command started as a coprocess.",
        SmartSelectionActionKind::SendText => "Text sent to the focused terminal.",
        SmartSelectionActionKind::RunCommandInWindow => "Command started in a new window.",
        SmartSelectionActionKind::Copy => "Empty copies the full match.",
    }
}

fn new_rule(existing: &[SmartSelectionRule]) -> SmartSelectionRule {
    SmartSelectionRule {
        id: next_rule_id(existing),
        name: "New rule".to_owned(),
        enabled: true,
        regex: r"\S+".to_owned(),
        precision: SmartSelectionPrecision::Normal,
        actions: vec![SmartSelectionAction::default()],
    }
}

fn next_rule_id(rules: &[SmartSelectionRule]) -> String {
    (1..=rules.len().saturating_add(1))
        .map(|index| format!("custom_{index}"))
        .find(|candidate| rules.iter().all(|rule| rule.id != *candidate))
        .unwrap_or_else(|| "custom_rule".to_owned())
}

fn parse_key(key: &str) -> Option<SmartControlKey> {
    if key == ACTIVATION_KEY {
        return Some(SmartControlKey::Activation);
    }
    if key == PREVIEW_KEY {
        return Some(SmartControlKey::Preview);
    }
    if key == PREVIEW_CURSOR_KEY {
        return Some(SmartControlKey::PreviewCursor);
    }
    let rest = key.strip_prefix(&format!("{PREFIX}rules."))?;
    let mut parts = rest.split('.');
    let rule = parts.next()?.parse().ok()?;
    match parts.next()? {
        "enabled" if parts.next().is_none() => Some(SmartControlKey::RuleEnabled(rule)),
        "name" if parts.next().is_none() => Some(SmartControlKey::RuleName(rule)),
        "regex" if parts.next().is_none() => Some(SmartControlKey::RuleRegex(rule)),
        "precision" if parts.next().is_none() => Some(SmartControlKey::RulePrecision(rule)),
        "actions" => {
            let action = parts.next()?.parse().ok()?;
            match parts.next()? {
                "kind" if parts.next().is_none() => {
                    Some(SmartControlKey::ActionKind { rule, action })
                }
                "parameter" if parts.next().is_none() => {
                    Some(SmartControlKey::ActionParameter { rule, action })
                }
                "parameter_mode" if parts.next().is_none() => {
                    Some(SmartControlKey::ActionMode { rule, action })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn option_label(options: &'static [(&'static str, &'static str)], token: &str) -> &'static str {
    options
        .iter()
        .find(|(value, _)| *value == token)
        .map_or(token_label_fallback(token), |row| row.1)
}

fn token_label_fallback(token: &str) -> &'static str {
    match token {
        "" => "Not set",
        _ => "Unknown",
    }
}

fn activation_token(value: SmartSelectionActivation) -> &'static str {
    match value {
        SmartSelectionActivation::DoubleClick => "double_click",
        SmartSelectionActivation::QuadClick => "quad_click",
    }
}

fn precision_token(value: SmartSelectionPrecision) -> &'static str {
    match value {
        SmartSelectionPrecision::VeryLow => "very_low",
        SmartSelectionPrecision::Low => "low",
        SmartSelectionPrecision::Normal => "normal",
        SmartSelectionPrecision::High => "high",
        SmartSelectionPrecision::VeryHigh => "very_high",
    }
}

fn action_kind_token(value: SmartSelectionActionKind) -> &'static str {
    match value {
        SmartSelectionActionKind::OpenFile => "open_file",
        SmartSelectionActionKind::OpenUrl => "open_url",
        SmartSelectionActionKind::RunCommand => "run_command",
        SmartSelectionActionKind::RunCoprocess => "run_coprocess",
        SmartSelectionActionKind::SendText => "send_text",
        SmartSelectionActionKind::RunCommandInWindow => "run_command_in_window",
        SmartSelectionActionKind::Copy => "copy",
    }
}

fn parameter_mode_token(value: SmartSelectionParameterMode) -> &'static str {
    match value {
        SmartSelectionParameterMode::Legacy => "legacy",
        SmartSelectionParameterMode::Interpolated => "interpolated",
    }
}

fn parse_activation(value: &Value) -> Result<SmartSelectionActivation, String> {
    match value.as_str() {
        Some("double_click") => Ok(SmartSelectionActivation::DoubleClick),
        Some("quad_click") => Ok(SmartSelectionActivation::QuadClick),
        _ => Err("unknown activation gesture".to_owned()),
    }
}

fn parse_precision(value: &Value) -> Result<SmartSelectionPrecision, String> {
    match value.as_str() {
        Some("very_low") => Ok(SmartSelectionPrecision::VeryLow),
        Some("low") => Ok(SmartSelectionPrecision::Low),
        Some("normal") => Ok(SmartSelectionPrecision::Normal),
        Some("high") => Ok(SmartSelectionPrecision::High),
        Some("very_high") => Ok(SmartSelectionPrecision::VeryHigh),
        _ => Err("unknown precision".to_owned()),
    }
}

fn parse_action_kind(value: &Value) -> Result<SmartSelectionActionKind, String> {
    match value.as_str() {
        Some("open_file") => Ok(SmartSelectionActionKind::OpenFile),
        Some("open_url") => Ok(SmartSelectionActionKind::OpenUrl),
        Some("run_command") => Ok(SmartSelectionActionKind::RunCommand),
        Some("run_coprocess") => Ok(SmartSelectionActionKind::RunCoprocess),
        Some("send_text") => Ok(SmartSelectionActionKind::SendText),
        Some("run_command_in_window") => Ok(SmartSelectionActionKind::RunCommandInWindow),
        Some("copy") => Ok(SmartSelectionActionKind::Copy),
        _ => Err("unknown action kind".to_owned()),
    }
}

fn parse_parameter_mode(value: &Value) -> Result<SmartSelectionParameterMode, String> {
    match value.as_str() {
        Some("legacy") => Ok(SmartSelectionParameterMode::Legacy),
        Some("interpolated") => Ok(SmartSelectionParameterMode::Interpolated),
        _ => Err("unknown parameter mode".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SmartActionTarget, action_kind_key, apply_action, apply_control_value, control_value,
        preview_match, rule_name_key, selected_rule_index,
    };
    use scribe_common::config::SmartSelectionConfig;
    use serde_json::json;

    #[test]
    fn dynamic_keys_round_trip_editor_values() {
        let mut config = SmartSelectionConfig::default();
        let mut preview = String::new();
        let mut cursor = 0;
        let key = rule_name_key(0);

        assert_eq!(
            control_value(&config, &preview, cursor, &key),
            Some(json!("Whitespace-bounded word"))
        );
        assert!(
            apply_control_value(&mut config, &mut preview, &mut cursor, &key, &json!("Token"),)
                .unwrap()
        );
        assert_eq!(config.rules[0].name, "Token");

        let action_key = action_kind_key(2, 0);
        assert!(
            apply_control_value(
                &mut config,
                &mut preview,
                &mut cursor,
                &action_key,
                &json!("copy"),
            )
            .unwrap()
        );
        assert_eq!(control_value(&config, &preview, cursor, &action_key), Some(json!("copy")));
    }

    #[test]
    fn add_duplicate_move_and_remove_keep_a_valid_selection() {
        let mut config = SmartSelectionConfig::default();
        let original = config.rules.len();
        let selected = apply_action(&mut config, 0, SmartActionTarget::AddRule);
        assert_eq!(config.rules.len(), original + 1);
        assert_eq!(selected, original);

        let selected = apply_action(&mut config, selected, SmartActionTarget::DuplicateRule);
        assert_eq!(config.rules.len(), original + 2);
        assert_eq!(selected, original + 1);

        let selected = apply_action(&mut config, selected, SmartActionTarget::MoveRuleUp);
        assert_eq!(selected, original);
        let selected = apply_action(&mut config, selected, SmartActionTarget::RemoveRule);
        assert_eq!(selected_rule_index(selected, config.rules.len()), Some(original));
    }

    #[test]
    fn preview_reports_match_at_the_cursor_and_invalid_regex() {
        let sample = "open https://example.com now";
        assert_eq!(
            preview_match(r"https?://\S+", sample, 10).unwrap(),
            Some("https://example.com".to_owned())
        );
        assert_eq!(preview_match(r"https?://\S+", sample, 0).unwrap(), None);
        assert!(preview_match("[", "anything", 0).unwrap_err().starts_with("Invalid regex:"));
    }
}
