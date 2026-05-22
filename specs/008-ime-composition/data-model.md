# Data Model: IME Composition and Preedit Handling

**Phase 1 output for `008-ime-composition`.** No new persistent or wire-
level entities. All state is client-local and transient.

---

## Entities

### `PreeditState` (new, client-local)

Per-window record of an in-progress composition. At most one instance
exists at any moment; lives in `App` and is owned by the currently focused
pane.

**Fields**:

| Field | Type (conceptual) | Purpose |
|---|---|---|
| `text` | `String` | Current preedit text from the most recent `Ime::Preedit` event. UTF-8. |
| `caret` | `Option<(usize, usize)>` | Byte-range caret hint from `Ime::Preedit`, when the IME reports a sub-range (e.g., the active segment in multi-segment composition). |
| `start_row` | `usize` | Absolute scrollback row where composition began. Used to keep the preedit anchored even if the underlying grid scrolls. |
| `start_col` | `usize` | Column at composition start. |

**Invariants**:

- `text` is never written to the PTY, scrollback, or any persisted state.
- `caret` byte indices, when present, lie within `text.len()`.
- `start_row`/`start_col` are captured on first `Ime::Preedit` after
  `Ime::Enabled` and remain stable until clear.
- A `PreeditState` only exists while `ime_active == true`.

**Lifecycle**:

```text
                  Ime::Enabled
                       │
   None  ───────────────────► ime_active = true
    ▲                            │
    │            Ime::Preedit(non-empty) ──► PreeditState created or updated
    │                            │
    │            Ime::Preedit("") OR Ime::Commit(text)
    │                            │
    │            (cancel or commit) ────────► PreeditState = None
    │                            │
    │            Ime::Disabled ──┘
    │                            │
    └────────────────────────────┘
         (also cleared on focus loss / pane change)
```

---

### `ImeActivationGate` (conceptual, not a struct)

A predicate combining existing focus state with no new fields:

```text
ime_should_be_allowed = window_focused
                      && x11_focus_guard_says_active   (X11 only)
                      && current_focused_surface == TerminalPane
```

The third clause excludes surfaces that explicitly opt out of IME in v1:
search overlay, modal dialogs (`update_dialog`, `close_dialog`,
`context_menu`). When the gate transitions, the corresponding
`Window::set_ime_allowed(bool)` call fires once.

---

### `TerminalMode` (existing — unchanged)

The bundle that carries Kitty progressive-enhancement flags + DECCKM /
DECPAM. **Not extended by this feature.** IME state lives separately on the
`App` so the encoder path is untouched.

---

### `Pane` (existing — minor addition)

The per-pane state already tracks input start, content dirty flags,
command records, etc. This feature adds **no fields** to `Pane`. Preedit
is window-global; rendering reads the focused pane's cursor cell from
existing accessors.

> Rationale: keeping preedit out of `Pane` avoids serialising it during
> snapshot/reattach and avoids the temptation to model preedit as per-
> session state. It's transient UI, not session state.

---

## State Machine

```text
                ┌─────────────────────────┐
                │      IME Disabled       │ ◄── default at startup
                │  (set_ime_allowed=false)│     and on window unfocus
                └──────────┬──────────────┘
                           │ pane gains focus AND gate passes
                           ▼
                ┌─────────────────────────┐
       ┌────────│      IME Enabled        │
       │        │   (allowed=true, no     │
       │        │     preedit yet)        │
       │        └──────────┬──────────────┘
       │                   │ Ime::Preedit(text)
       │                   ▼
       │        ┌─────────────────────────┐
       │        │      Composing          │
       │        │   PreeditState = Some   │
       │        └──────┬─────────┬────────┘
       │               │         │
       │   Ime::Commit │         │ Ime::Preedit("")
       │               │         │   OR Ime::Disabled
       │               ▼         ▼
       │        ┌─────────────────────────┐
       └────────│   Cleared (preedit=None)│
                └─────────────────────────┘
                pane unfocus → IME Disabled
                window unfocus → IME Disabled
```

**Transitions**:

| From | Event | To | Side effects |
|---|---|---|---|
| Disabled | Gate-pass on pane focus | Enabled | `set_ime_allowed(true)`, push initial cursor area |
| Enabled | `Ime::Preedit(text != "")` | Composing | Create `PreeditState`, mark redraw dirty |
| Composing | `Ime::Preedit(text)` | Composing | Update text/caret, mark redraw dirty |
| Composing | `Ime::Preedit("")` | Enabled | Drop `PreeditState`, mark redraw dirty |
| Composing | `Ime::Commit(text)` | Enabled | Drop `PreeditState`; send `ClientMessage::KeyInput { bytes = text.bytes }` |
| Enabled / Composing | `Ime::Disabled` | Disabled | Drop `PreeditState` if any |
| Enabled / Composing | Focus lost / pane changed | Disabled | `set_ime_allowed(false)`, drop `PreeditState` |
| Any | Resize / DPI / cursor cell move | (unchanged) | Push updated cursor area via `set_ime_cursor_area` |

---

## No-Change Surfaces

The following are explicitly NOT modified:

- `ClientMessage` / `ServerMessage` / `UiEvent` / `protocol.rs` —
  committed text reuses `KeyInput`.
- `config.rs` — no new keys.
- `session_manager.rs`, `pty/*` — server side unchanged.
- `screen.rs` / `screen_replay.rs` — preedit is never serialized.
- `level-4 encoders` (`translate_key`, `translate_key_kitty`,
  `translate_numpad_app_keypad`) — bypassed, not modified.

---

## Validation Rules (from spec FRs)

- `PreeditState` MUST be cleared before any commit text reaches the PTY
  (FR-007).
- `PreeditState` MUST be cleared on `WindowEvent::Focused(false)` and on
  pane focus change (FR-008).
- Commit text MUST be routed as UTF-8 bytes, not as encoder output (FR-004,
  R5).
- Preedit rendering MUST NOT modify the underlying terminal grid (FR-006,
  FR-010).
- `set_ime_allowed` MUST follow the activation gate predicate (FR-001,
  FR-011, FR-012).
