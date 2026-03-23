# Implement Missing Settings

**Date**: 2026-03-23
**Status**: Approved

## Summary

Approximately 50% of Scribe's config settings are parsed and stored but never applied.
This spec covers wiring all unimplemented settings through to the renderer, input
handler, and server, plus removing the dead `terminal.shell` field.

### Settings inventory

| Setting | Status | Action |
|---------|--------|--------|
| `appearance.font` | Not implemented | Wire to GlyphAtlas |
| `appearance.font_weight` | Not implemented | Wire to build_attrs |
| `appearance.font_weight_bold` | Not implemented | Wire to build_attrs |
| `appearance.ligatures` | Not implemented | Toggle Shaping mode |
| `appearance.line_padding` | Not implemented | Add to line_height |
| `appearance.cursor_shape` | Not implemented | Beam + underline rendering |
| `appearance.cursor_blink` | Not implemented | Timer + reset-on-keypress |
| `appearance.opacity` | Not implemented | Window transparency + clear color alpha |
| `terminal.shell` | Dead config | Remove entirely |
| `keybindings.*` (5 fields) | Ignored | Parse strings, match dynamically |

### Decisions made

- **Cursor blink**: Full implementation with timer + reset-on-keypress (like xterm/VTE).
- **Keybindings**: Single-combo parser only. Designed so adding new actions is just an enum
  variant + config field — no parser changes.
- **Opacity**: Only enable `with_transparent(true)` when `opacity < 1.0` at startup. If
  opacity changes from 1.0 at runtime, log a warning that restart is required. No compositor
  detection — if no compositor, window renders opaque silently.
- **Font family**: Validate font exists via cosmic-text font database. Log warning and fall
  back to `Family::Monospace` if not found.
- **Ligatures**: `Shaping::Advanced` when enabled, `Shaping::Basic` when disabled.
  (cosmic-text 0.17 uses `Shaping::Basic`, not `Simple`.)
- **`terminal.shell`**: Removed. `$SHELL` env var already controls this via
  `alacritty_terminal::tty`.

## Implementation order

1. Remove `terminal.shell`
2. Font family
3. Font weight / bold weight
4. Ligatures
5. Line padding
6. Cursor shape
7. Cursor blink
8. Opacity
9. Keybindings
10. Hot-reload consolidation

---

## 1. Remove `terminal.shell`

### Files changed

- `crates/scribe-common/src/config.rs` — remove `shell: String` from `TerminalConfig`,
  remove from `Default` impl
- `crates/scribe-client/src/main.rs` — remove `"terminal.shell"` arm from `apply_config_key`
- `crates/scribe-settings/src/assets/settings.html` — remove shell input element
- `crates/scribe-settings/src/assets/settings.js` — remove `setTextValue("terminal.shell", ...)`

### Rationale

`$SHELL` already controls which shell is spawned. `PtyOptions::default()` respects it.
No code ever reads `config.terminal.shell`. Dead config is worse than no config.

---

## 2. Font family

### Files changed

- `crates/scribe-renderer/src/atlas.rs` — new constructor parameter, store family name,
  use `Family::Name(family)` in `build_attrs()` and `measure_cell()`
- `crates/scribe-renderer/src/lib.rs` — pass font family from config to `GlyphAtlas::new()`
  and `rebuild_atlas()`
- `crates/scribe-client/src/main.rs` — pass `config.appearance.font` to renderer

### Font validation

On atlas creation, validate the font exists by calling `font_system.db().query()` with
the requested family name, configured weight, `Stretch::Normal`, and `Style::Normal`.
This matches what the shaping path will actually request. If `query()` returns `None`:

```
WARN font "Nonexistent Font" (weight 400) not found, falling back to system monospace
```

Fall back to `Family::Monospace`. The `measure_cell()` function must also use the
validated family (not hardcoded `Family::Monospace`) — it receives the family via
`FontParams` just like `build_attrs()`.

### FontParams struct

Introduce a bundled parameter struct to avoid unbounded constructor growth:

```rust
pub struct FontParams {
    pub family: String,
    pub size: f32,
    pub weight: u16,
    pub weight_bold: u16,
    pub ligatures: bool,
    pub line_padding: u16,
}
```

Used by `GlyphAtlas::new()` and `rebuild_atlas()`.

### Bind group rebuild

`rebuild_atlas()` creates a new `wgpu::Texture`, but the pipeline's bind group still
references the old texture handle. After rebuilding the atlas, the pipeline bind group
must be recreated:

```rust
pub fn rebuild_atlas(&mut self, device: &Device, queue: &Queue, params: &FontParams) {
    self.atlas = GlyphAtlas::new(device, queue, params);
    self.cell_size = self.atlas.cell_size();
    self.grid_size = compute_grid_size(self.viewport_size, self.cell_size);
    // Rebind the pipeline to the new atlas texture
    self.pipeline.rebuild_bind_group(device, self.atlas.texture_view(), self.atlas.sampler());
}
```

This is a pre-existing latent bug — the current `rebuild_atlas` (used for font_size
hot-reload) also fails to rebind. This fix addresses both the existing and new code paths.

---

## 3. Font weight / bold weight

### Files changed

- `crates/scribe-renderer/src/atlas.rs` — store `font_weight` and `font_weight_bold`,
  use in `build_attrs()`

### Change

```rust
// Before
.weight(if key.bold { Weight::BOLD } else { Weight::NORMAL })

// After
.weight(Weight(if key.bold { self.font_weight_bold } else { self.font_weight }))
```

Values come from `FontParams`. cosmic-text's `Weight` wraps a `u16` (100-900 scale).

---

## 4. Ligatures

### Files changed

- `crates/scribe-renderer/src/atlas.rs` — store `ligatures: bool`, select shaping mode

### Change

```rust
let shaping = if self.ligatures { Shaping::Advanced } else { Shaping::Basic };
```

Applied in both `rasterize_rgba` (via `shape_cache_key` → `set_text`) and `measure_cell`.

---

## 5. Line padding

### Files changed

- `crates/scribe-renderer/src/atlas.rs` — add padding to line height calculation

### Change

```rust
// Before
let line_height = font_size * 1.2;

// After
let line_height = font_size * 1.2 + f32::from(line_padding);
```

The glyph is still rasterized at `font_size` — only cell height grows. Glyph stays
vertically centered because `dest_y` is computed from `metrics.font_size`, not `line_height`.

Propagates automatically through `CellSize` → `compute_grid_size` → instance positioning
→ PTY resize.

---

## 6. Cursor shape

### Files changed

- `crates/scribe-renderer/src/lib.rs` — store `CursorShape`, modify cursor rendering in
  `build_instances_offset`
- `crates/scribe-common/src/config.rs` — `CursorShape` already defined (Block/Beam/Underline)

### Three styles

**Block** (existing): Invert fg/bg for the entire cursor cell. No change.

**Beam**: Cell renders with normal colors. An additional `CellInstance` is pushed covering
the leftmost portion of the cell — zeroed glyph UVs (solid), cursor color as fg and bg.
Width: `max(2.0, cell_width / 8.0)` pixels to remain visible on HiDPI displays.

**Underline**: Same approach — additional instance covering the bottom portion of the cell.
Height: `max(2.0, cell_height / 8.0)` pixels for the same DPI-aware reason.

### Cursor color

`theme.cursor` is already parsed in theme resolution. Store it in the renderer alongside
`default_fg` / `default_bg`:

```rust
cursor_color: [f32; 4],  // from theme, converted to linear
```

`set_theme()` must be updated to store this alongside the existing default_fg/default_bg:

```rust
self.cursor_color = srgb_to_linear_rgba(theme.cursor);
```

---

## 7. Cursor blink

### Files changed

- `crates/scribe-client/src/main.rs` — blink state, timer, reset-on-keypress,
  `ControlFlow::WaitUntil`

### State (on client App struct)

```rust
cursor_visible: bool,    // toggled by timer
cursor_blink: bool,      // from config
blink_timer: Instant,    // last toggle time
```

### Blink cadence

530ms interval (matches xterm/VTE). Checked each frame:

```rust
if self.cursor_blink && self.blink_timer.elapsed() >= BLINK_INTERVAL {
    self.cursor_visible = !self.cursor_visible;
    self.blink_timer = Instant::now();
}
```

When `cursor_visible` is false, cursor cell renders with normal colors (no overlay/invert).

### Reset on keypress

After any keypress in the input handling path:

```rust
self.cursor_visible = true;
self.blink_timer = Instant::now();
```

### Redraw scheduling

Implement `about_to_wait` on `ApplicationHandler<UiEvent>` to set the control flow.
The app already uses a background thread that sends `UiEvent::AnimationTick` via
`EventLoopProxy` — cursor blink must coexist with this.

Strategy: in `about_to_wait`, if cursor blink is active, call
`event_loop.set_control_flow(ControlFlow::WaitUntil(next_blink_time))`. The event loop
wakes at whichever deadline comes first — the `WaitUntil` time or an incoming user event
(including `AnimationTick` from the proxy). This means both mechanisms coexist naturally.

When blink is disabled and no animation is active, use `ControlFlow::Wait`.

### Hot-reload transitions

- Enabled to disabled: set `cursor_visible = true`, revert to `ControlFlow::Wait`
- Disabled to enabled: reset timer, `about_to_wait` picks up the new deadline next frame

---

## 8. Opacity

### Files changed

- `crates/scribe-client/src/main.rs` — window creation, clear color, cell bg alpha

### Window creation

At startup, check `config.appearance.opacity`:
- `< 1.0`: create window with `with_transparent(true)`, store opacity
- `== 1.0`: create window normally

### Surface alpha mode

When transparency is active, `configure_device_and_surface` must select
`CompositeAlphaMode::PreMultiplied` (or `PostMultiplied`) instead of defaulting to
whatever `caps.alpha_modes.first()` returns. If the surface is configured with
`CompositeAlphaMode::Opaque`, alpha manipulation has no visible effect.

Check available modes from `surface.get_capabilities(adapter).alpha_modes` and prefer
`PreMultiplied` > `PostMultiplied` > `Opaque` (with a warning). When transparency is
not active, `Opaque` is fine.

### Clear color

```rust
clear_color: wgpu::Color { r, g, b, a: f64::from(self.opacity) }
```

Cell instance background alpha is multiplied by opacity so opaque cell backgrounds
don't punch through the transparent window. Foreground (glyph) alpha stays at 1.0 —
only backgrounds become translucent. This matches the behavior of most terminal
emulators (kitty, Alacritty) where text remains fully opaque over a see-through
background.

### No compositor fallback

No detection. If no compositor is running, the window renders opaque. Log at info:

```
window transparency enabled (opacity=0.9)
```

### Hot-reload caveat

If window was created without `with_transparent(true)` (opacity was 1.0 at startup) and
user changes opacity to < 1.0 via settings:

```
WARN opacity < 1.0 requires restart to take effect (window was created without transparency)
```

If window was created with transparency, opacity changes apply immediately.

---

## 9. Keybindings

### Files changed

- `crates/scribe-client/src/input.rs` — `Keybinding` struct, parser, `matches()` method,
  `Bindings` struct, refactored `translate_layout_shortcut`
- `crates/scribe-client/src/main.rs` — store `Bindings`, pass to input handler

### Data structures

```rust
pub struct Keybinding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: KeyMatch,
}

pub enum KeyMatch {
    Character(char),
    Named(NamedKey),
}

pub struct Bindings {
    pub split_vertical: Keybinding,
    pub split_horizontal: Keybinding,
    pub close_pane: Keybinding,
    pub cycle_pane: Keybinding,
    pub settings: Keybinding,
}
```

### Parser

`Keybinding::parse(s: &str) -> Option<Keybinding>`:
- Split on `+`
- Identify modifiers: `ctrl`, `shift`, `alt`
- Final segment is the key — map to `KeyMatch::Named` for known names (`tab`, `enter`,
  `space`), otherwise `KeyMatch::Character`

### Matching

`Keybinding::matches(&self, event: &KeyEvent, modifiers: ModifiersState) -> bool` compares
modifier flags and key.

`translate_layout_shortcut` becomes:

```rust
fn translate_layout_shortcut(event: &KeyEvent, modifiers: ModifiersState, bindings: &Bindings) -> Option<LayoutAction> {
    if bindings.split_vertical.matches(event, modifiers) { return Some(SplitVertical); }
    if bindings.split_horizontal.matches(event, modifiers) { return Some(SplitHorizontal); }
    // ...
}
```

Settings shortcut (`Ctrl+,`) also matched dynamically via `bindings.settings`.

### Invalid config

If a keybinding string can't be parsed, log a warning and use the default for that action.

### Extensibility

Adding a new action: add `LayoutAction` variant, `KeybindingsConfig` field,
`Bindings` field, match arm. No parser changes.

---

## 10. Hot-reload consolidation

### Files changed

- `crates/scribe-client/src/main.rs` — expanded `handle_config_changed()`

### Rebuild strategy

**Atlas rebuild** (expensive, one rebuild even if multiple font fields changed):
- `font`, `font_size`, `font_weight`, `font_weight_bold`, `ligatures`, `line_padding`

**Cheap state update** (set field, next frame picks it up):
- `cursor_shape`, `cursor_blink`, `theme`

**Restart required**:
- `opacity` changing from 1.0 when window was created without transparency

### Implementation

```rust
fn handle_config_changed(&mut self) {
    let new_config = load_config()?;
    let old = &self.config;

    // Theme
    let theme_changed = old.appearance.theme != new_config.appearance.theme;
    if theme_changed {
        let new_theme = resolve_theme(&new_config);
        gpu.renderer.set_theme(&new_theme);
        self.theme = new_theme;
    }

    // Font params — single atlas rebuild for any change
    let font_changed = old.appearance.font != new_config.appearance.font
        || (old.appearance.font_size - new_config.appearance.font_size).abs() > f32::EPSILON
        || old.appearance.font_weight != new_config.appearance.font_weight
        || old.appearance.font_weight_bold != new_config.appearance.font_weight_bold
        || old.appearance.ligatures != new_config.appearance.ligatures
        || old.appearance.line_padding != new_config.appearance.line_padding;

    if font_changed {
        let params = FontParams::from(&new_config.appearance);
        gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
        // Recompute grid size, send resize to server
    }

    // Cursor
    gpu.renderer.set_cursor_shape(new_config.appearance.cursor_shape);
    self.cursor_blink = new_config.appearance.cursor_blink;

    // Opacity
    if opacity_changed_and_needs_restart {
        tracing::warn!("opacity < 1.0 requires restart");
    } else {
        self.opacity = new_config.appearance.opacity;
    }

    // Keybindings
    self.bindings = Bindings::parse(&new_config.keybindings);

    self.config = new_config;
    self.request_redraw();
}
```

---

## Out of scope

- Multi-key chord keybindings (future enhancement)
- Compositor detection for opacity
- `workspaces.add_root` file picker (already tracked separately)
- Sub-pixel anti-aliasing (cosmic-text SubpixelMask)
- Keybinding editing UI in settings webview (users edit `config.toml` directly for now)
