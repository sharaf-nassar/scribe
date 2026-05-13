# Data Model: Smart Selection

## SmartSelectionConfig

Global terminal preference group that controls Smart Selection behavior.

Fields:
- `activation`: `SmartSelectionActivation`
- `rules`: ordered list of `SmartSelectionRule`

Validation:
- `activation` must be `double_click` or `quad_click`.
- `rules` may be empty, but reset restores the default rules.
- Disabled rules remain persisted but do not participate in matching or context-menu actions.

Defaults:
- `activation`: `quad_click`
- `rules`: default recognizers for whitespace-bounded words, namespace identifiers, filesystem paths, quoted strings, include paths, URIs, Objective-C-style selectors, and email addresses.

## SmartSelectionActivation

Gesture that invokes Smart Selection.

Values:
- `double_click`: double click invokes Smart Selection instead of ordinary word selection.
- `quad_click`: double click keeps ordinary word selection and four quick clicks invoke Smart Selection.

Validation:
- Unknown values fall back to `quad_click` when loading legacy or edited configuration.

## SmartSelectionRule

User-editable rule that can produce Smart Selection candidates.

Fields:
- `id`: stable string identifier for settings edits and UI list keys
- `name`: display name
- `enabled`: boolean
- `regex`: regular expression pattern
- `precision`: `SmartSelectionPrecision`
- `actions`: ordered list of `SmartSelectionAction`

Validation:
- `name` must not be empty in the settings UI; generated defaults use descriptive names.
- `regex` must compile before the rule participates in matching.
- Invalid regexes are shown in settings and skipped at click time.
- `precision` must be one of the five defined precision levels.
- Rule IDs must be unique within the rule list.

## SmartSelectionPrecision

Relative confidence level used to rank matching candidates.

Values:
- `very_low`
- `low`
- `normal`
- `high`
- `very_high`

Candidate ranking:
- For each rule, use the longest match containing the cursor character.
- Prefer candidates from the highest precision class with any match.
- Within that precision class, choose the longest candidate.

## SmartSelectionCandidate

Transient match result produced during selection or context-menu lookup.

Fields:
- `rule_id`: rule that produced the candidate
- `text`: full matched text
- `range`: start and end terminal grid coordinates
- `captures`: full match plus capture groups
- `precision`: rule precision
- `actions`: actions associated with the rule

Validation:
- The match must include the cursor character.
- The grid range must map back to visible terminal cells.
- Captures missing from an optional group remain available as empty values for parameter expansion.

## SmartSelectionAction

User-invoked operation associated with a rule.

Fields:
- `kind`: `SmartSelectionActionKind`
- `parameter`: string interpreted according to `kind`
- `parameter_mode`: `legacy` or `interpolated`

Validation:
- Actions must not execute during selection.
- Actions appear only when their rule matches the context-menu cursor location.
- Parameter expansion must be deterministic even when current directory, user, or host is unavailable.

## SmartSelectionActionKind

Supported action types.

Values:
- `open_file`
- `open_url`
- `run_command`
- `run_coprocess`
- `send_text`
- `run_command_in_window`
- `copy`

Validation:
- `open_url` only opens accepted URL schemes.
- `open_file` resolves paths using available working-directory context.
- `copy` writes the expanded parameter, or the full match when the parameter is empty.
- Command-like and send-text actions require explicit user invocation.

## Parameter Expansion

Action parameters can reference match and session context.

Legacy placeholders:
- `\0`: full match
- `\1` through `\9`: capture groups
- `\d`: current directory
- `\u`: user name
- `\h`: host name
- `\n`: newline
- `\\`: literal backslash

Interpolated values:
- `matches[0]`: full match
- `matches[1]` and higher: capture groups
- `path`: current directory
- `user`: user name
- `host`: host name

Validation:
- Missing capture groups expand to an empty string.
- Missing session context expands to an empty string and should not hide the action.
