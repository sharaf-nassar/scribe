# Contract: Smart Selection Settings UI

## Scope

This contract defines the user-facing controls required in the Terminal settings page.

## Section Placement

The Terminal page includes a dedicated section labeled `Smart Selection`.

The section appears after the General selection controls and before Status Bar controls so selection behavior remains grouped together.

## Required Controls

Activation control:
- Segmented control with `Double Click` and `Quad Click`.
- Default visible value is `Quad Click`.

Rule list:
- Shows rule enabled state, name, precision, action count, and validation state.
- Supports add, duplicate, remove, move up/down, enable/disable, and restore defaults.
- Selecting a rule opens an editor for that rule.

Rule editor:
- Name text field.
- Regular expression text field or multiline field.
- Precision menu with Very Low, Low, Normal, High, Very High.
- Test text area.
- Test cursor position input or click-in-sample affordance.
- Test result preview showing selected text or validation error.

Action editor:
- Ordered list of actions for the selected rule.
- Action kind menu with Open File, Open URL, Run Command, Run Coprocess, Send Text, Run Command in Window, Copy.
- Parameter field.
- Parameter mode control with Legacy and Interpolated.
- Add, remove, duplicate, and reorder actions.

## Save Behavior

Every durable change sends either:
- `terminal.smart_selection` with the full config payload, or
- `terminal.smart_selection.reset` with `true`.

Invalid regex changes:
- Must show an inline error.
- Must not silently remove the rule.
- Must not break other valid rules.

## Empty States

If all rules are removed:
- The rule list shows an empty state.
- Smart Selection invokes no match until the user adds a rule or restores defaults.
- Restore defaults remains available.

If a selected rule has no actions:
- The context-menu action preview shows no rule-specific actions.
- Selection still works for that rule.

## Accessibility and Interaction

Controls must be keyboard reachable in a predictable order. Destructive actions such as removing a rule should require a clear button action and must not be triggered by merely selecting a rule.
