# Contract: Smart Selection Config

## Scope

This contract defines the persisted terminal settings shape for global Smart Selection configuration.

## TOML Shape

Smart Selection lives under the terminal section.

```toml
[terminal.smart_selection]
activation = "quad_click"

[[terminal.smart_selection.rules]]
id = "default-url"
name = "URL"
enabled = true
regex = "..."
precision = "very_high"

[[terminal.smart_selection.rules.actions]]
kind = "open_url"
parameter = "\\0"
parameter_mode = "legacy"
```

## Activation Values

Accepted values:
- `double_click`
- `quad_click`

Default:
- `quad_click`

Behavior:
- `double_click` replaces ordinary double-click word selection.
- `quad_click` preserves ordinary double-click word selection and assigns Smart Selection to four quick clicks.

## Rule Values

Required rule fields:
- `id`: unique stable identifier
- `name`: user-facing label
- `enabled`: boolean
- `regex`: regular expression string
- `precision`: one of the precision values
- `actions`: zero or more action entries

Precision values:
- `very_low`
- `low`
- `normal`
- `high`
- `very_high`

Validation:
- Invalid regexes remain visible in settings but are skipped by terminal click handling.
- Empty names are rejected by settings UI before saving.
- Duplicate IDs are normalized by the settings UI before saving.

## Action Values

Action kinds:
- `open_file`
- `open_url`
- `run_command`
- `run_coprocess`
- `send_text`
- `run_command_in_window`
- `copy`

Parameter modes:
- `legacy`
- `interpolated`

Validation:
- Unknown action kinds are ignored at runtime and flagged in settings.
- Missing parameter defaults to an empty string.
- Command-like actions never run unless the user explicitly selects the context-menu action.

## Settings Webview JSON Updates

The settings UI may update the whole Smart Selection config in one message:

```json
{
  "type": "setting_changed",
  "key": "terminal.smart_selection",
  "value": {
    "activation": "quad_click",
    "rules": [
      {
        "id": "default-url",
        "name": "URL",
        "enabled": true,
        "regex": "...",
        "precision": "very_high",
        "actions": [
          {
            "kind": "open_url",
            "parameter": "\\0",
            "parameter_mode": "legacy"
          }
        ]
      }
    ]
  }
}
```

The settings UI may also request reset to defaults:

```json
{
  "type": "setting_changed",
  "key": "terminal.smart_selection.reset",
  "value": true
}
```

## Compatibility

Existing config files without `terminal.smart_selection` load with default Smart Selection settings. Existing terminal settings keep their current keys and behavior.
