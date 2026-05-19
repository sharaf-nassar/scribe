# Phase 1 Data Model: Keyboard Protocol & Command Awareness

All entities are **client-side, in-memory** types in `scribe-client` plus two additive
`scribe-common` config fields. No serialized/persisted schema changes (the IPC protocol and
cold-restart snapshot are unchanged).

## Track A — Keyboard

### `KittyFlags` (new, `scribe-client/src/input.rs`)
The per-session set of negotiated Kitty progressive-enhancement flags. Replaces the
two-variant `KeyboardProtocol` enum.

| Field | Type | Source |
|-------|------|--------|
| `disambiguate` | `bool` | `TermMode::DISAMBIGUATE_ESC_CODES` |
| `report_event_types` | `bool` | `TermMode::REPORT_EVENT_TYPES` |
| `report_alternate_keys` | `bool` | `TermMode::REPORT_ALTERNATE_KEYS` |
| `report_all_keys` | `bool` | `TermMode::REPORT_ALL_KEYS_AS_ESC` |
| `report_associated_text` | `bool` | `TermMode::REPORT_ASSOCIATED_TEXT` |

- **Derivation**: built each keystroke by `focused_keyboard_protocol()` from the focused
  pane's `Term::mode()`; forced all-`false` when `keyboard_protocol_enhanced == false`.
- **Lifecycle**: transient (recomputed per event); the authoritative push/pop stack lives in
  alacritty `Term`. No Scribe-side persistence.
- **Validation/invariant**: all-`false` ⇒ byte-identical legacy encoding (SC-003). Per-pane:
  only the focused pane's mode is read (SC-008).

### Key-event encoding (transient, no struct required)
The CSI-u output computed per keystroke. Form:
`ESC [ <codepoint> [ : <alternate-codepoints> ] [ ; <modifiers> [ : <event-type> ] ]
[ ; <associated-text> ] u`.

| Element | Rule |
|---------|------|
| codepoint | Unicode value, or functional-key number from the new `NamedKey→u32` table |
| modifiers | `1 + shift(1) + alt(2) + ctrl(4) + super(8)` (existing xterm formula) |
| event-type | `1` press \| `2` repeat (`Pressed && event.repeat`) \| `3` release (`Released`); emitted only if `report_event_types` |
| alternate | base (`key_without_modifiers()`) + shifted (`logical_key`); only if `report_alternate_keys` |
| associated text | `event.text` codepoints; only if `report_associated_text` |

### `TerminalConfig.keyboard_protocol_enhanced` (new, `scribe-common/src/config.rs`)
`bool`, `#[serde(default = "default_true")]`, default `true`. Master opt-out (FR-006).
Additive — old configs deserialize unchanged.

## Track B — Command Awareness

### `CommandStatus` (new enum, `scribe-client/src/pane.rs`)
`Success` (exit 0) \| `Failure` (exit ≠ 0) \| `Unknown` (no `exit_code` resolved before the
next prompt). **Invariant**: `Unknown` MUST never render with failure styling (FR-012/SC-006).

### `CommandRecord` (new, `scribe-client/src/pane.rs`)
| Field | Type | Meaning |
|-------|------|---------|
| `abs_pos` | `usize` | absolute scrollback row of the `PromptStart` (A) — same unit/role as today's `prompt_marks` entries |
| `status` | `CommandStatus` | resolved outcome |

- **Replaces** `Pane::prompt_marks: Vec<usize>` with `Pane::command_records:
  Vec<CommandRecord>`.
- **Trim/shift**: `shift_absolute_marks_after_trim` retargets to mutate `record.abs_pos`
  (and drop records below `dropped_rows`) — reuses the proven mechanism (FR-013/SC-007).
- **Lifecycle**: per-pane; appended on `A`; resolved on `D`; pruned like today; **reset
  empty on reattach/handoff** (replay carries no OSC 133 callbacks — non-misleading per
  FR-014).

### `Pane.last_command_status: Option<CommandStatus>` (new)
Most-recently-resolved outcome, fed to the status bar (FR-009/SC-004). `None` until the first
`D` resolves.

### State transitions (per pane, driven by `handle_prompt_mark`)
```
        A (PromptStart)              D (CommandEnd, exit_code)
 idle ───────────────▶ Unknown ─────────────────────────────▶ Success | Failure | Unknown
   ▲                      │  exit 0→Success, ≠0→Failure, None→Unknown
   │                      │
   └──────────────────────┘  next A before D ⇒ prior record stays Unknown
 B (PromptEnd) / C (CommandStart): clear input_start only (unchanged); no status effect
 D with no open record: ignored
```

### `UiEvent::PromptMark` (modified, `scribe-client/src/ipc_client.rs`)
Add `exit_code: Option<i32>`. **Client-internal only** — `UiEvent` is never serialized;
`ServerMessage::PromptMark` already carries `exit_code`, so this is a Rust struct change with
no wire/version impact (migration: none).

### `StatusBarData.last_command_status: Option<CommandStatus>` (new field)
Drives a left-segment colored indicator (`✓`/`✗`/neutral `?`) following the existing
`connected_dot` pattern.

### `KeybindingsConfig.jump_to_failure` (new, `scribe-common/src/config.rs`)
`KeyComboList` with a distinct default + `default_jump_to_failure()`. Additive; existing
`prompt_jump_up`/`prompt_jump_down` keep their keys and command-boundary semantics.

## Entity → Requirement / Story map

| Entity | Requirements | Story |
|--------|--------------|-------|
| `KittyFlags` | FR-001, FR-003, FR-004, FR-005 | US1 |
| Key-event encoding | FR-001, FR-002, FR-003 | US1 |
| `keyboard_protocol_enhanced` | FR-006 | US1 |
| `CommandStatus` / `CommandRecord` | FR-007, FR-008, FR-012, FR-013 | US2 |
| `last_command_status` (Pane + StatusBarData) | FR-009 | US2 |
| `UiEvent::PromptMark.exit_code` | FR-007 | US2 |
| `command_records` nav + `jump_to_failure` | FR-010, FR-011 | US3 |
| reattach reset behavior | FR-014 | US2/US3 |
