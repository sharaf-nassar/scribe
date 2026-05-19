# Contract: Config & Keybinding Additions

User-facing `~/.config/scribe/config.toml` surface. Both additions are **additive and
backward-compatible** — old config files load unchanged (Constitution compatibility
decision: no migration, no version bump).

## `[terminal]` — `keyboard_protocol_enhanced`

```toml
[terminal]
keyboard_protocol_enhanced = true   # default; new key
```

- Type `bool`, `#[serde(default = "default_true")]`, default **`true`**.
- `true`: applications may negotiate the full Kitty keyboard protocol (this feature).
- `false`: hard opt-out — `KittyFlags` forced all-`false`, pure legacy encoding regardless of
  what the application requests (FR-006). A safety hatch for misbehaving apps.
- Lives alongside existing `[terminal]` keys (`scrollback_lines`, `copy_on_select`, …);
  applied live via the existing config-reload path (no restart).

## `[keybindings]` — `jump_to_failure`

```toml
[keybindings]
jump_to_failure = "ctrl+shift+g"    # new action; default is a distinct, unused combo
# existing, unchanged:
prompt_jump_up   = "ctrl+shift+z"
prompt_jump_down = "ctrl+shift+x"
```

- Type `KeyComboList` (string or array of ≤5 combos), same shape as every other action.
- Default: a distinct combo not colliding with existing defaults (final value chosen in
  implementation; documented in README/settings like other bindings).
- Action: scroll the focused pane to the **most recent `Failure`** command record. When none
  exists → no-op with a non-disruptive signal, consistent with the existing
  top/bottom-of-scroll jump no-op (FR-011).
- `prompt_jump_up`/`prompt_jump_down` keep their keys and now navigate command-boundary
  records (status-agnostic) — behavior for users unchanged (anchored on PromptStart rows).

## Settings UI

The opt-out and the new binding appear in the existing settings webview Terminal/Keybindings
pages using the established controls (Constitution UX consistency). No new settings paradigm.
