# Contracts: Paste Confirmation (Multiline / Control-Character)

This feature exposes no external/network interface. Its "contracts" are the
internal surfaces other parts of Scribe (and the user's config) depend on: the
config key, the settings round-trip, the dialog action model, and the gate
decision rule. **There is NO protocol/IPC/server contract change** (research
R8).

## C1 — Config key

| Property | Value |
|----------|-------|
| TOML key | `terminal.paste_confirmation` |
| Type | boolean |
| Default | `false` (absent key deserializes to `false` via `#[serde(default)]`) |
| Meaning | `true` enables the paste gate; `false` = today's behavior |
| Compatibility | additive, backward + forward compatible; no migration (research R7) |

## C2 — Settings webview round-trip

- **Control**: a Terminal-page toggle with `data-key="terminal.paste_confirmation"`.
- **Load**: `setToggleValue("terminal.paste_confirmation", config.terminal?.paste_confirmation ?? false)`.
- **Change**: the generic toggle click handler emits the existing IPC message
  `{ "type": "setting_changed", "key": "terminal.paste_confirmation", "value": <bool> }`.
- **Apply**: `apply_settings_change` → `apply_config_key` → `apply_terminal_key`
  (new match arm) → `apply_terminal_behavior_key` parses the bool and assigns
  `config.terminal.paste_confirmation`, then `save_config` persists.
- **Reload**: file watcher → `ConfigReloaded` updates `App.config`; the gate
  reads `self.config.terminal.paste_confirmation` at paste time, so the change
  takes effect on the **next paste** with no restart.

No new IPC message types are introduced; this reuses the existing
`setting_changed` channel.

## C3 — Dialog action model

```text
PasteConfirmationAction = Paste | Cancel
```

| Input | Result |
|-------|--------|
| Click **Paste** / `Enter` while Paste focused | resume: deliver parked content to the parked target (bracketed-wrap + chunk via the existing send tail), bypassing the gate |
| Click **Cancel** / `Esc` / `Enter` while Cancel focused | drop the parked paste; 0 bytes to PTY |
| `Tab` / `Shift+Tab` | cycle button focus |
| any key/click while open | captured by the dialog; never leaks to the PTY (FR-010) |

- **Default focus**: `Cancel` (index 0) — safe default, mirrors the
  disallowed-scheme dialog.
- **Modality**: same one-modal-at-a-time convention as the existing
  Close/Update/Clipboard/Disallowed-scheme dialogs.

## C4 — Gate decision rule (authoritative truth table)

Inputs at paste time, for **non-empty** content:

| `paste_confirmation` enabled | focused pane bracketed | `classify_paste(text)` | Outcome |
|:---:|:---:|:---:|---|
| false | — | — | **send immediately** (unchanged path) |
| true | true | — | **send immediately** (defer to bracketed paste — "match modern shell behavior", FR-003) |
| true | false | `None` (no line break, no non-tab control) | **send immediately** |
| true | false | `Some(risk)` | **confirm** — open `PasteConfirmationDialog`, park content+target |

`classify_paste(text) -> Some(PasteRisk)` iff the text contains `\n`/`\r`
(`has_line_break`) **or** any `char::is_control()` other than `\t`/`\n`/`\r`
(`has_control`); otherwise `None`. Empty content sends nothing regardless.

## C5 — Delivery invariant

On **Paste**, the bytes delivered to the PTY MUST be byte-identical to what the
same paste would deliver with the feature disabled (SC-002): the confirmation
neither transforms, reorders, truncates, nor sanitizes content. The preview
shown in the dialog is a *display-only* caret-escaped rendering and never
affects delivered bytes.
