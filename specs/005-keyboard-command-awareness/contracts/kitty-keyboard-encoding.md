# Contract: Kitty Keyboard Protocol Outbound Encoding

**Surface**: bytes `scribe-client` writes to the PTY (via `ClientMessage::KeyInput`) for a
key event, when the focused application has negotiated the protocol. The authoritative
external contract is the published Kitty keyboard protocol; this documents the subset Scribe
guarantees and the winit-sourced inputs.

## Activation

- Encoding applies **only** at level 4 of the key-translation chain (after layout shortcuts,
  palette/settings/find, and terminal shortcuts decline the key).
- Gated by `KittyFlags` derived from the focused pane `Term::mode()`. If
  `keyboard_protocol_enhanced = false` OR no flag is negotiated → **legacy encoding,
  byte-identical to today** (SC-003). Per-pane: only the focused pane's mode is consulted.

## Sequence form

`CSI <codepoint> [: <shifted> [: <base-layout>]] [; <modifiers> [: <event-type>]]
[; <text-codepoints>] u`

| Field | When emitted | Value |
|-------|--------------|-------|
| codepoint | always | Unicode codepoint, or functional-key number (new `NamedKey → u32` table) |
| modifiers | when modifiers held, or any non-press event, or required by flag | `1 + Shift(1) + Alt(2) + Ctrl(4) + Super(8)` |
| event-type | only if `report_event_types` | `1`=press, `2`=repeat (`Pressed && event.repeat`), `3`=release (`Released`) |
| shifted / base-layout | only if `report_alternate_keys` | shifted from `logical_key`; base from `key_without_modifiers()` |
| text-codepoints | only if `report_associated_text` | `event.text` Unicode scalars |

## Per-flag obligations

| Flag (`CSI = N u`) | Obligation |
|---|---|
| `1` disambiguate | Modified/ambiguous keys (incl. `Ctrl+I` vs `Tab`, bare `Esc`) emit distinct CSI-u; unmodified printable text unchanged |
| `2` report event types | Emit repeat (`2`) and release (`3`) events; gate widened on terminal path only |
| `4` report alternate keys | Append shifted/base codepoints |
| `8` report all keys as escapes | Even unmodified non-text keys emit CSI-u |
| `16` report associated text | Append associated-text field |

Only negotiated flags take effect; un-negotiated flags MUST NOT alter output (FR-003).
Push/pop nesting is honored automatically (state lives in alacritty `Term`; recomputed per
keystroke).

## Non-regression guarantees

- No flags negotiated ⇒ every byte identical to pre-feature output (SC-003).
- Bracketed paste, dead keys, IME composition paths unchanged (separate code paths).
- The Codex `Alt+Enter` override continues to fire before generic encoding and stays
  coherent (no double-encode) for that key.

## Verification

Manual: run a negotiating app (e.g. `kitty +kitten show_key -m kitty`, or Neovim/Helix with a
key-logger) and confirm each row of the protocol's key+modifier matrix, plus repeat/release
when negotiated. See `quickstart.md` US1.
