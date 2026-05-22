# Contract: IME Event Pipeline

**Phase 1 contract for `008-ime-composition`.** This feature does not
expose any external (cross-process, cross-host, or library-public)
interface. The contract documented here is the **internal pipeline
contract** between `winit` events, the client's input subsystem, the
renderer, and the existing PTY write path.

External callers (the server, other crates, the CLI) see no new surface.

---

## Inbound surface (consumed by Scribe)

### `WindowEvent::Ime(Ime::Enabled)`

- **When**: OS reports the window is now IME-eligible after
  `set_ime_allowed(true)`.
- **Scribe action**:
  - Set `app.ime_active = true`.
  - Push initial cursor area for the focused pane.
- **Side effects**: No PTY bytes. No grid mutation.

### `WindowEvent::Ime(Ime::Preedit(text, caret))`

- **When**: OS reports in-progress composition.
- **Inputs**:
  - `text: String` — current preedit text (UTF-8).
  - `caret: Option<(usize, usize)>` — optional byte range inside `text`
    marking the IME's caret / selection.
- **Scribe action**:
  - If `text.is_empty()`: clear `PreeditState`.
  - Else: create or update `PreeditState { text, caret, start_row,
    start_col }`. The first non-empty `Preedit` after a clear captures
    the current cursor cell as the anchor.
  - Mark the focused pane's redraw dirty.
- **Side effects**: No PTY bytes. No grid mutation.

### `WindowEvent::Ime(Ime::Commit(text))`

- **When**: OS reports finalized composition.
- **Inputs**: `text: String` — committed UTF-8 text (already past IME
  processing).
- **Scribe action**:
  - Clear `PreeditState`.
  - Send `ClientMessage::KeyInput { session_id: focused_pane.session_id,
    bytes: text.into_bytes() }` to the server.
  - Mark redraw dirty.
- **Side effects**: PTY receives the bytes via the existing server
  `KeyInput` write path. No encoder transformation. No bracketed-paste
  markers.

### `WindowEvent::Ime(Ime::Disabled)`

- **When**: OS reports IME deactivation (typically follows
  `set_ime_allowed(false)`, or the user switching to a non-IME input
  source).
- **Scribe action**:
  - Set `app.ime_active = false`.
  - Clear `PreeditState`.
  - Mark redraw dirty.

### `WindowEvent::KeyboardInput` (existing)

- **Modified behavior** (one-line check at the start of dispatch):
  - If `app.ime_active && key_was_consumed_by_ime`, return early before
    invoking the level-4 encoder or any shortcut layer.
  - Otherwise, dispatch unchanged through the existing four-level
    pipeline.
- **No change** to `translate_key`, `translate_key_kitty`,
  `translate_numpad_app_keypad`, layout shortcuts, palette dispatch, or
  any other downstream key consumer.

### `WindowEvent::Focused(bool)` (existing)

- **Modified behavior**:
  - On `Focused(true)`: re-evaluate the activation gate; if it passes,
    call `set_ime_allowed(true)` and push cursor area.
  - On `Focused(false)`: call `set_ime_allowed(false)` and clear
    `PreeditState`.

### `WindowEvent::Resized` / `WindowEvent::ScaleFactorChanged` (existing)

- **Modified behavior**: After existing handling, push an updated
  `set_ime_cursor_area` for the focused pane's cursor cell.

### Focused-pane change (existing internal event)

- **Modified behavior**: Clear `PreeditState`, re-evaluate gate, push
  cursor area.

---

## Outbound surface (Scribe → OS via winit)

### `Window::set_ime_allowed(bool)`

- **Called on**: activation-gate transition.
- **Contract**: Single source of truth for IME enablement.

### `Window::set_ime_cursor_area(position, size)`

- **Called on**: focus change, cursor cell movement (frame-coupled),
  resize, DPI change.
- **Contract**:
  - `position` = window-space origin of the cursor cell (top-left), in
    points.
  - `size` = `(cell_width, cell_height)` of the focused pane in points.
  - Coordinate space matches winit's window coordinates after
    `ScaleFactorChanged` adjustments.

---

## Internal contract: `App` ↔ Renderer

### Preedit overlay request

- **Producer**: `App` populates a per-frame `PreeditOverlay` struct in
  the renderer input, computed from `PreeditState` and the focused
  pane's current cursor cell.
- **Consumer**: Renderer draws preedit text via existing cosmic-text
  shaping; draws underline via `chrome.rs#solid_quad`.
- **Layer order**: Below search overlay, dialogs, context menu; above
  the terminal grid.
- **Lifecycle**: Recomputed every frame; no caching across frames.

### Cursor-rect query (renderer → App)

- **Existing**: The renderer already exposes the cursor cell's
  window-space rectangle each frame (for cursor rendering).
- **Reuse**: `App` reads this on each redraw to call
  `set_ime_cursor_area`. No new accessor required.

---

## Out-of-band: PTY response

The PTY may emit terminal sequences in response to committed text (echo,
shell rendering, command output). All such output flows through the
existing `ServerMessage::PtyOutput` → renderer path with no IME-specific
handling. From the PTY's perspective, the IME-committed bytes are
indistinguishable from native keyboard input.

---

## Versioning / Compatibility

- **Protocol version**: unchanged. `KeyInput` already exists.
- **Settings schema**: unchanged.
- **Session persistence**: unchanged. Preedit is never serialized.
- **Server rollout**: not required. Client-only change.

---

## Failure modes & fallbacks

| Failure | Behavior |
|---|---|
| Wayland compositor lacks `zwp_text_input_v3` | `Ime::Enabled` never arrives; user sees current behavior (no IME). Documented as platform limitation. |
| User has no IME configured | `Ime::Enabled` may still arrive but no preedit events fire. ASCII typing unchanged. |
| winit emits `Ime::Commit` without prior `Enabled` | Treat as a one-shot commit; route bytes; do not assert. |
| `Ime::Preedit` arrives while window unfocused (synthetic) | Ignored; activation gate blocks. |
| Empty `Ime::Commit("")` | No-op; no bytes sent. |
| Multi-codepoint commit with embedded NUL bytes | Bytes sent as-is; PTY semantics define behavior. |
