# Implement Missing Settings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire all 15 unimplemented config settings through to the renderer, input handler, and window — plus remove dead `terminal.shell` config.

**Architecture:** Each setting flows through a consistent path: config struct (scribe-common) → renderer/client/input (scribe-renderer, scribe-client) → hot-reload handler (scribe-client main.rs). Font-related settings bundle into a `FontParams` struct. Cursor blink uses `about_to_wait` for event loop scheduling. Keybindings get a parse-once/match-per-event model.

**Tech Stack:** Rust, wgpu, cosmic-text 0.17, winit 0.30, alacritty_terminal

**Spec:** `docs/superpowers/specs/2026-03-23-implement-missing-settings-design.md`

**Lint policy:** `unsafe_code` denied, strict clippy (pedantic + restriction lints), `#[allow]` requires `reason = "..."`, cognitive complexity 15, function params 5, lines 80, nesting 4.

---

## File Map

| File | Role | Tasks |
|------|------|-------|
| `crates/scribe-common/src/config.rs` | Config structs | 1 |
| `crates/scribe-renderer/Cargo.toml` | Renderer dependencies | 2 |
| `crates/scribe-renderer/src/atlas.rs` | Glyph atlas (font rendering) | 2 |
| `crates/scribe-renderer/src/lib.rs` | Terminal renderer | 2, 3 |
| `crates/scribe-renderer/src/pipeline.rs` | wgpu pipeline (bind group, instance layout) | 3 |
| `crates/scribe-renderer/src/types.rs` | GPU instance data (`CellInstance`) | 3 |
| `crates/scribe-renderer/src/shaders/terminal.wgsl` | Vertex/fragment shader | 3 |
| `crates/scribe-client/src/input.rs` | Keyboard input + keybindings | 6 |
| `crates/scribe-client/src/main.rs` | Client app, event loop, hot-reload | 1, 2, 3, 4, 5, 7 |
| `crates/scribe-settings/src/assets/settings.html` | Settings UI HTML | 1 |
| `crates/scribe-settings/src/assets/settings.js` | Settings UI JS | 1 |

---

### Task 1: Remove `terminal.shell`

**Files:**
- Modify: `crates/scribe-common/src/config.rs:138-148`
- Modify: `crates/scribe-client/src/main.rs:1328-1330`
- Modify: `crates/scribe-settings/src/assets/settings.html:398-404`
- Modify: `crates/scribe-settings/src/assets/settings.js:249`

- [ ] **Step 1: Remove `shell` field from `TerminalConfig`**

In `crates/scribe-common/src/config.rs`, remove the `shell: String` field from the `TerminalConfig` struct (line 142) and its `Default` impl (line 147). The struct becomes:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { scrollback_lines: default_scrollback_lines() }
    }
}
```

- [ ] **Step 2: Remove `"terminal.shell"` from `apply_config_key`**

In `crates/scribe-client/src/main.rs`, remove the `"terminal.shell"` match arm (lines 1328-1330).

- [ ] **Step 3: Remove shell input from settings UI**

In `crates/scribe-settings/src/assets/settings.html`, remove the shell setting row (lines 398-404):
```html
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-label">Shell</div>
              <div class="setting-desc">Path to shell executable</div>
            </div>
            <input class="text-input" data-key="terminal.shell" type="text" value="/bin/bash" placeholder="/bin/bash">
          </div>
```

In `crates/scribe-settings/src/assets/settings.js`, remove line 249:
```js
  setTextValue("terminal.shell", config.terminal?.shell);
```

- [ ] **Step 4: Verify build**

Run: `cargo check --workspace`
Expected: Clean build, no errors.

- [ ] **Step 5: Commit**

```bash
git add crates/scribe-common/src/config.rs crates/scribe-client/src/main.rs \
  crates/scribe-settings/src/assets/settings.html crates/scribe-settings/src/assets/settings.js
git commit -m "refactor: remove dead terminal.shell config setting

\$SHELL env var already controls shell selection via alacritty_terminal.
No code ever consumed this config field."
```

---

### Task 2: Font family + FontParams + bind group fix

**Files:**
- Modify: `crates/scribe-renderer/Cargo.toml` (add `fontdb` dependency)
- Modify: `crates/scribe-renderer/src/atlas.rs:80-144, 265-270, 377-400`
- Modify: `crates/scribe-renderer/src/lib.rs:23-47, 185-191`
- Modify: `crates/scribe-client/src/main.rs:256-261`

- [ ] **Step 0: Add `fontdb` dependency to scribe-renderer**

In `crates/scribe-renderer/Cargo.toml`, add `fontdb` as a dependency. Check the version used by cosmic-text 0.17 (likely 0.23.x) and match it:

```bash
cargo metadata --format-version=1 | jq -r '.packages[] | select(.name == "cosmic-text") | .dependencies[] | select(.name == "fontdb") | .req'
```

Add the matching version to `[dependencies]` in `crates/scribe-renderer/Cargo.toml`.

- [ ] **Step 1: Add `FontParams` struct to `atlas.rs`**

At the top of `crates/scribe-renderer/src/atlas.rs` (after the imports), add:

```rust
/// Bundled font configuration for atlas construction.
///
/// Groups all font-related settings so the constructor signature stays stable
/// as new font settings are added.
#[derive(Debug, Clone)]
pub struct FontParams {
    /// Font family name (e.g. "JetBrains Mono"). Validated against the font database.
    pub family: String,
    /// Base font size in pixels.
    pub size: f32,
    /// Font weight for normal text (100-900 scale).
    pub weight: u16,
    /// Font weight for bold text (100-900 scale).
    pub weight_bold: u16,
    /// Whether to enable OpenType ligatures.
    pub ligatures: bool,
    /// Extra vertical padding between lines in pixels.
    pub line_padding: u16,
}
```

- [ ] **Step 2: Add font fields to `GlyphAtlas` struct**

Add fields to the `GlyphAtlas` struct (line 80) to store the resolved family and font params:

```rust
pub struct GlyphAtlas {
    // ... existing fields ...
    /// Resolved font family — either `Family::Name(...)` or `Family::Monospace` fallback.
    family: Family<'static>,
    font_weight: u16,
    font_weight_bold: u16,
    ligatures: bool,
}
```

The `family` field stores the validated family. The `'static` lifetime requires leaking the family string (same pattern used in `build_theme_from_config` in `config.rs:352`).

- [ ] **Step 3: Update `GlyphAtlas::new()` to accept `FontParams`**

Change the constructor signature from `fn new(device, queue, font_size)` to `fn new(device, queue, params: &FontParams)`. Inside:

1. Create `FontSystem::new()`
2. Validate the font family: call `font_system.db().query(&fontdb::Query { families: &[fontdb::Family::Name(&params.family)], weight: fontdb::Weight(params.weight), stretch: fontdb::Stretch::Normal, style: fontdb::Style::Normal })`. If `None`, log a warning and use `Family::Monospace`. If `Some`, leak the family string to get `Family::Name(leaked_str)`.
3. Compute metrics with `line_padding`: `let line_height = params.size * 1.2 + f32::from(params.line_padding);`
4. Pass the resolved family to `measure_cell` (updated in next step)
5. Store `family`, `font_weight`, `font_weight_bold`, `ligatures` in `Self`

Note: `fontdb` types (`Query`, `Database`, `ID`) are NOT re-exported through `cosmic_text`. Use the `fontdb` crate directly (added in Step 0). The `FontSystem::db()` method returns a `&fontdb::Database`.

- [ ] **Step 4: Update `build_attrs` to use stored family and weights**

Change `build_attrs` from a free function to a method on `GlyphAtlas` (or pass family/weights as parameters). It must use:

```rust
fn build_attrs(&self, key: GlyphKey) -> Attrs<'static> {
    use cosmic_text::{Style, Weight};
    Attrs::new()
        .family(self.family)
        .weight(Weight(if key.bold { self.font_weight_bold } else { self.font_weight }))
        .style(if key.italic { Style::Italic } else { Style::Normal })
}
```

Update the call site in `shape_cache_key` (line 267): `let attrs = self.build_attrs(key);`

- [ ] **Step 5: Update `measure_cell` to accept family and shaping**

Change `measure_cell` to accept the resolved family and ligatures flag:

```rust
fn measure_cell(font_system: &mut FontSystem, metrics: Metrics, family: Family<'_>, ligatures: bool) -> CellSize {
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(family);
    let shaping = if ligatures { Shaping::Advanced } else { Shaping::Basic };
    buf.set_text(font_system, "M", &attrs, shaping, None);
    // ... rest unchanged
}
```

- [ ] **Step 6: Update `shape_cache_key` to use configured shaping**

In `shape_cache_key` (line 265-276), use the stored `ligatures` flag:

```rust
let shaping = if self.ligatures { Shaping::Advanced } else { Shaping::Basic };
buf.set_text(&mut self.font_system, text, &attrs, shaping, None);
```

- [ ] **Step 7: Update `TerminalRenderer::new()` to accept `FontParams`**

In `crates/scribe-renderer/src/lib.rs`, change the constructor (line 40):

```rust
pub fn new(
    device: &Device,
    queue: &Queue,
    surface_format: TextureFormat,
    params: &FontParams,
    viewport_size: (u32, u32),
) -> Self {
    let atlas = GlyphAtlas::new(device, queue, params);
    // ... rest unchanged
}
```

- [ ] **Step 8: Fix `rebuild_atlas` to accept `FontParams` and rebuild bind group**

In `crates/scribe-renderer/src/lib.rs`, change `rebuild_atlas` (line 187):

```rust
pub fn rebuild_atlas(&mut self, device: &Device, queue: &Queue, params: &FontParams) {
    self.atlas = GlyphAtlas::new(device, queue, params);
    self.cell_size = self.atlas.cell_size();
    self.grid_size = compute_grid_size(self.viewport_size, self.cell_size);
    self.pipeline.rebuild_bind_group(device, self.atlas.texture_view(), self.atlas.sampler());
}
```

This also fixes a pre-existing latent bug where the pipeline bind group was not rebuilt after atlas recreation.

- [ ] **Step 9: Update client `main.rs` to pass `FontParams`**

In `crates/scribe-client/src/main.rs`, update `init_gpu_and_terminal` (around line 256) to construct `FontParams` from config and pass it to `TerminalRenderer::new()`:

```rust
let font_params = scribe_renderer::atlas::FontParams {
    family: self.config.appearance.font.clone(),
    size: self.config.appearance.font_size,
    weight: self.config.appearance.font_weight,
    weight_bold: self.config.appearance.font_weight_bold,
    ligatures: self.config.appearance.ligatures,
    line_padding: self.config.appearance.line_padding,
};
let mut renderer = TerminalRenderer::new(&device, &queue, surface_config.format, &font_params, (size.width, size.height));
```

- [ ] **Step 10: Verify build + lint**

Run: `cargo clippy --workspace`
Expected: Clean. Watch for `clippy::too_many_arguments` (we introduced `FontParams` to avoid this), `allow_attributes_without_reason`, and any `cast` warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/scribe-renderer/Cargo.toml crates/scribe-renderer/src/atlas.rs \
  crates/scribe-renderer/src/lib.rs crates/scribe-client/src/main.rs
git commit -m "feat: wire font family, weight, ligatures, line_padding to renderer

Introduce FontParams struct to bundle font settings for the glyph atlas.
Font family is validated against fontdb; falls back to system monospace.
Weights use configured values instead of hardcoded BOLD/NORMAL.
Ligatures toggle between Shaping::Advanced and Shaping::Basic.
Line padding adds to line_height in metrics calculation.
Also fixes latent bug: rebuild_atlas now calls pipeline.rebuild_bind_group."
```

---

### Task 3: Cursor shape

The vertex shader (`terminal.wgsl`) uses `uniforms.cell_size` to size ALL instances
identically. Beam and underline cursors need sub-cell-sized quads, so we must add a
per-instance `size` override to `CellInstance` and update the shader accordingly.

**Files:**
- Modify: `crates/scribe-renderer/src/types.rs:1-10` (add `size` to `CellInstance`)
- Modify: `crates/scribe-renderer/src/shaders/terminal.wgsl:10-34` (per-instance size)
- Modify: `crates/scribe-renderer/src/pipeline.rs:251-265` (instance buffer layout)
- Modify: `crates/scribe-renderer/src/lib.rs:23-33, 62-80, 148-165, 203-246` (cursor rendering)
- Modify: `crates/scribe-client/src/main.rs` (propagate `cursor_visible` through `build_all_instances`)

- [ ] **Step 1: Add `size` field to `CellInstance`**

In `crates/scribe-renderer/src/types.rs`, add a `size` field to `CellInstance`:

```rust
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CellInstance {
    pub pos: [f32; 2],
    pub size: [f32; 2],    // NEW: [0,0] means "use cell_size uniform"
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub fg_color: [f32; 4],
    pub bg_color: [f32; 4],
}
```

- [ ] **Step 2: Update the shader**

In `crates/scribe-renderer/src/shaders/terminal.wgsl`, add the `size` field to `CellInstance` and use it in the vertex shader:

```wgsl
struct CellInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) fg_color: vec4<f32>,
    @location(5) bg_color: vec4<f32>,
};

@vertex fn vs_main(
    @builtin(vertex_index) vi: u32,
    instance: CellInstance,
) -> VertexOutput {
    // ... corners array unchanged ...
    let corner = corners[vi];
    // Use per-instance size if non-zero, otherwise fall back to cell_size uniform
    let quad_size = select(uniforms.cell_size, instance.size, instance.size.x > 0.0);
    let pixel_pos = instance.pos + corner * quad_size;
    // ... rest unchanged ...
}
```

- [ ] **Step 3: Update instance buffer layout**

In `crates/scribe-renderer/src/pipeline.rs`, update `instance_buffer_layout()` to include
the new `size` attribute. Shift all subsequent `shader_location` values by 1 and adjust
`offset` values (each `Float32x2` is 8 bytes):

```rust
const ATTRS: &[VertexAttribute] = &[
    VertexAttribute { format: VertexFormat::Float32x2, offset: 0, shader_location: 0 },   // pos
    VertexAttribute { format: VertexFormat::Float32x2, offset: 8, shader_location: 1 },   // size
    VertexAttribute { format: VertexFormat::Float32x2, offset: 16, shader_location: 2 },  // uv_min
    VertexAttribute { format: VertexFormat::Float32x2, offset: 24, shader_location: 3 },  // uv_max
    VertexAttribute { format: VertexFormat::Float32x4, offset: 32, shader_location: 4 },  // fg_color
    VertexAttribute { format: VertexFormat::Float32x4, offset: 48, shader_location: 5 },  // bg_color
];
```

The `array_stride` uses `size_of::<CellInstance>()` so it updates automatically.

- [ ] **Step 4: Update all `CellInstance` construction sites**

Every place that creates a `CellInstance` must now include `size: [0.0, 0.0]` (which means
"use the cell_size uniform" per the shader). Search for `CellInstance {` across the codebase
and add the field. The main sites are in `lib.rs` (`build_instances_offset`) and any chrome
rendering code.

- [ ] **Step 5: Add cursor fields to `TerminalRenderer`**

In `crates/scribe-renderer/src/lib.rs`, add to the struct (after line 32):

```rust
cursor_shape: scribe_common::config::CursorShape,
cursor_color: [f32; 4],
```

Initialize in `new()`:

```rust
cursor_shape: scribe_common::config::CursorShape::Block,
cursor_color: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
```

- [ ] **Step 6: Store cursor color in `set_theme`**

In `set_theme` (line 148), add:

```rust
self.cursor_color = srgb_to_linear_rgba(theme.cursor);
```

- [ ] **Step 7: Add `set_cursor_shape` method**

```rust
pub fn set_cursor_shape(&mut self, shape: scribe_common::config::CursorShape) {
    self.cursor_shape = shape;
}
```

- [ ] **Step 8: Add `cursor_visible` parameter through the call chain**

Add `cursor_visible: bool` to:
1. `build_instances_at` (in `lib.rs`)
2. `build_instances_offset` (in `lib.rs`)
3. `build_all_instances` (free function in `main.rs`, around line 1040)

Pass `cursor_visible` from `handle_redraw` → `build_all_instances` → `build_instances_at` → `build_instances_offset`.

- [ ] **Step 9: Implement beam and underline cursor rendering**

In `build_instances_offset`, replace the block cursor logic (lines 235-237):

```rust
let is_cursor = cursor_visible
    && point.line == cursor_point.line
    && point.column == cursor_point.column;

if is_cursor {
    match self.cursor_shape {
        CursorShape::Block => {
            // Swap fg/bg for the whole cell
            instances.push(CellInstance {
                pos, size: [0.0, 0.0], uv_min, uv_max,
                fg_color: bg, bg_color: fg,
            });
        }
        CursorShape::Beam => {
            // Normal cell first
            instances.push(CellInstance {
                pos, size: [0.0, 0.0], uv_min, uv_max,
                fg_color: fg, bg_color: bg,
            });
            // Beam overlay: thin vertical bar, explicit sub-cell size
            let beam_w = f32::max(2.0, cell_w / 8.0);
            instances.push(CellInstance {
                pos,
                size: [beam_w, cell_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: self.cursor_color,
                bg_color: self.cursor_color,
            });
        }
        CursorShape::Underline => {
            // Normal cell first
            instances.push(CellInstance {
                pos, size: [0.0, 0.0], uv_min, uv_max,
                fg_color: fg, bg_color: bg,
            });
            // Underline overlay: thin horizontal bar at bottom
            let ul_h = f32::max(2.0, cell_h / 8.0);
            instances.push(CellInstance {
                pos: [pos[0], pos[1] + cell_h - ul_h],
                size: [cell_w, ul_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: self.cursor_color,
                bg_color: self.cursor_color,
            });
        }
    }
} else {
    instances.push(CellInstance {
        pos, size: [0.0, 0.0], uv_min, uv_max,
        fg_color: fg, bg_color: bg,
    });
}
```

The beam/underline overlays use zeroed UVs (sampling the transparent-black pixel at atlas
origin). With both fg and bg set to cursor_color, `mix(bg, fg, 0.0) = bg = cursor_color`.
The per-instance `size` field makes the quad the correct dimensions.

- [ ] **Step 10: Verify build + lint**

Run: `cargo clippy --workspace`

- [ ] **Step 11: Commit**

```bash
git add crates/scribe-renderer/src/types.rs crates/scribe-renderer/src/shaders/terminal.wgsl \
  crates/scribe-renderer/src/pipeline.rs crates/scribe-renderer/src/lib.rs \
  crates/scribe-client/src/main.rs
git commit -m "feat: implement beam and underline cursor shapes

Add per-instance size override to CellInstance and shader, enabling
sub-cell-sized quads for cursor overlays. Block cursor remains the
default. Beam draws a thin vertical bar; underline a thin horizontal
bar. Both use DPI-aware sizing: max(2px, cell_dim / 8).
Cursor color sourced from theme.cursor via set_theme."
```

---

### Task 4: Cursor blink

**Files:**
- Modify: `crates/scribe-client/src/main.rs:51-101, 147-200, 490-511`

- [ ] **Step 1: Add blink state to `App`**

In `crates/scribe-client/src/main.rs`, add to the `App` struct (after `last_tick` field, line 95):

```rust
/// Whether the cursor is currently visible (toggled by blink timer).
cursor_visible: bool,
/// Whether cursor blinking is enabled (from config).
cursor_blink_enabled: bool,
/// Time of last blink toggle.
blink_timer: Instant,
```

Initialize in `App::new()`:

```rust
cursor_visible: true,
cursor_blink_enabled: config.appearance.cursor_blink,
blink_timer: Instant::now(),
```

Add a constant near the top of the file:

```rust
/// Cursor blink interval (530ms matches xterm/VTE).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
```

- [ ] **Step 2: Implement `about_to_wait`**

Add to `impl ApplicationHandler<UiEvent> for App`:

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    if self.cursor_blink_enabled {
        let elapsed = self.blink_timer.elapsed();
        let remaining = BLINK_INTERVAL.saturating_sub(elapsed);
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + remaining));
    } else {
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}
```

Import `ControlFlow` from `winit::event_loop`.

- [ ] **Step 3: Add blink toggle logic to the render path**

In `handle_redraw` (or just before render), add the blink check:

```rust
if self.cursor_blink_enabled && self.blink_timer.elapsed() >= BLINK_INTERVAL {
    self.cursor_visible = !self.cursor_visible;
    self.blink_timer = Instant::now();
    self.request_redraw();
}
```

Pass `self.cursor_visible` to the renderer's `build_instances_at` (which passes it to `build_instances_offset`).

- [ ] **Step 4: Reset blink on keypress**

In the keyboard input handler (where `translate_key_action` is called), after any keypress that produces a `KeyAction`:

```rust
self.cursor_visible = true;
self.blink_timer = Instant::now();
```

- [ ] **Step 5: Verify build + lint**

Run: `cargo clippy --workspace`

- [ ] **Step 6: Commit**

```bash
git add crates/scribe-client/src/main.rs
git commit -m "feat: implement cursor blink with reset-on-keypress

530ms interval matches xterm/VTE. Cursor resets to visible on any
keypress. Uses about_to_wait with ControlFlow::WaitUntil for efficient
scheduling that coexists with the existing animation tick timer."
```

---

### Task 5: Opacity

**Files:**
- Modify: `crates/scribe-client/src/main.rs:51-101, 244-247, 616-617, 1100-1139`

- [ ] **Step 1: Add opacity state to `App`**

Add fields to the `App` struct:

```rust
/// Current opacity (0.0-1.0). Applied to clear color and cell backgrounds.
opacity: f32,
/// Whether the window was created with transparency support.
window_transparent: bool,
```

Initialize in `App::new()`:

```rust
opacity: config.appearance.opacity,
window_transparent: config.appearance.opacity < 1.0,
```

- [ ] **Step 2: Enable window transparency conditionally**

In `init_gpu_and_terminal` (line 246), modify window creation:

```rust
let mut attrs = Window::default_attributes().with_title("Scribe");
if self.window_transparent {
    attrs = attrs.with_transparent(true);
    tracing::info!(opacity = self.opacity, "window transparency enabled");
}
let window = Arc::new(event_loop.create_window(attrs).map_err(InitError::Window)?);
```

- [ ] **Step 3: Select correct `CompositeAlphaMode`**

In `configure_device_and_surface` (line 1101), add a `transparent: bool` parameter. When transparent, prefer `PreMultiplied` > `PostMultiplied`:

```rust
fn configure_device_and_surface(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
    window: &Window,
    transparent: bool,
) -> Result<(wgpu::Device, wgpu::Queue, wgpu::SurfaceConfiguration), InitError> {
    // ... adapter + device creation unchanged ...

    let caps = surface.get_capabilities(&adapter);
    let alpha_mode = if transparent {
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            wgpu::CompositeAlphaMode::PostMultiplied
        } else {
            tracing::warn!("no transparency-capable alpha mode available");
            caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
        }
    } else {
        caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
    };
    // ... rest unchanged, uses alpha_mode in config ...
}
```

Update the call site in `init_gpu_and_terminal` to pass `self.window_transparent`.

- [ ] **Step 4: Apply opacity to clear color and cell backgrounds**

In the render path (line 616-617), multiply the clear color alpha by opacity:

```rust
let mut clear_color = gpu.renderer.default_bg();
if let Some(a) = clear_color.get_mut(3) {
    *a *= self.opacity;
}
gpu.renderer.pipeline_mut().render_with_clear(&mut encoder, &view, clear_color);
```

For cell background alpha: add a method to `TerminalRenderer` that accepts opacity and applies it to each instance's `bg_color[3]` after `build_instances_at`, or pass opacity into the render method. The simplest approach is to post-process instances in the client:

```rust
if self.opacity < 1.0 {
    for inst in &mut instances {
        if let Some(a) = inst.bg_color.get_mut(3) {
            *a *= self.opacity;
        }
    }
}
```

- [ ] **Step 5: Verify build + lint**

Run: `cargo clippy --workspace`

- [ ] **Step 6: Commit**

```bash
git add crates/scribe-client/src/main.rs
git commit -m "feat: implement window opacity setting

Enables transparency only when opacity < 1.0 at startup. Uses
PreMultiplied alpha mode when available. Cell backgrounds get opacity
applied; foreground glyphs stay fully opaque. Changing from 1.0 at
runtime logs a restart-required warning."
```

---

### Task 6: Keybindings

**Files:**
- Modify: `crates/scribe-client/src/input.rs:1-108`
- Modify: `crates/scribe-client/src/main.rs:51-101, 147-200`

- [ ] **Step 1: Add keybinding data structures to `input.rs`**

At the top of `crates/scribe-client/src/input.rs`, add:

```rust
use scribe_common::config::KeybindingsConfig;

/// A parsed key match target.
#[derive(Debug, Clone)]
pub enum KeyMatch {
    /// A single character key (e.g. 'w', '\\', '-').
    Character(char),
    /// A named key (e.g. Tab, Enter).
    Named(NamedKey),
}

/// A parsed keybinding: modifier flags + key.
#[derive(Debug, Clone)]
pub struct Keybinding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: KeyMatch,
}

/// All parsed keybindings for layout and UI actions.
#[derive(Debug, Clone)]
pub struct Bindings {
    pub split_vertical: Keybinding,
    pub split_horizontal: Keybinding,
    pub close_pane: Keybinding,
    pub cycle_pane: Keybinding,
    pub settings: Keybinding,
}
```

- [ ] **Step 2: Implement `Keybinding::parse`**

```rust
impl Keybinding {
    /// Parse a keybinding string like "ctrl+shift+w" or "ctrl+tab".
    ///
    /// Returns `None` if the string is empty or the key segment is unrecognised.
    pub fn parse(s: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key_part = None;

        for part in s.split('+') {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                _ => key_part = Some(lower),
            }
        }

        let key = match key_part?.as_str() {
            "tab" => KeyMatch::Named(NamedKey::Tab),
            "enter" | "return" => KeyMatch::Named(NamedKey::Enter),
            "space" => KeyMatch::Named(NamedKey::Space),
            "backspace" => KeyMatch::Named(NamedKey::Backspace),
            "escape" | "esc" => KeyMatch::Named(NamedKey::Escape),
            "delete" => KeyMatch::Named(NamedKey::Delete),
            s if s.len() == 1 => KeyMatch::Character(s.chars().next()?),
            _ => return None,
        };

        Some(Self { ctrl, shift, alt, key })
    }
}
```

- [ ] **Step 3: Implement `Keybinding::matches`**

```rust
impl Keybinding {
    /// Check if this keybinding matches the given key event and modifiers.
    pub fn matches(&self, event: &KeyEvent, modifiers: ModifiersState) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        if self.ctrl != modifiers.control_key()
            || self.shift != modifiers.shift_key()
            || self.alt != modifiers.alt_key()
        {
            return false;
        }
        match &self.key {
            KeyMatch::Character(c) => {
                if let Key::Character(key_str) = &event.logical_key {
                    key_str.chars().next().is_some_and(|k| k.eq_ignore_ascii_case(c))
                } else {
                    false
                }
            }
            KeyMatch::Named(named) => {
                matches!(&event.logical_key, Key::Named(n) if n == named)
            }
        }
    }
}
```

- [ ] **Step 4: Implement `Bindings::parse`**

```rust
impl Bindings {
    /// Parse all keybindings from config, falling back to defaults for invalid entries.
    pub fn parse(config: &KeybindingsConfig) -> Self {
        Self {
            split_vertical: parse_or_default(&config.split_vertical, "ctrl+shift+\\"),
            split_horizontal: parse_or_default(&config.split_horizontal, "ctrl+shift+-"),
            close_pane: parse_or_default(&config.close_pane, "ctrl+shift+w"),
            cycle_pane: parse_or_default(&config.cycle_pane, "ctrl+tab"),
            settings: parse_or_default(&config.settings, "ctrl+,"),
        }
    }
}

fn parse_or_default(value: &str, default: &str) -> Keybinding {
    Keybinding::parse(value).unwrap_or_else(|| {
        tracing::warn!(binding = value, "invalid keybinding, using default: {default}");
        // Default parse should never fail — these are known-good strings.
        #[allow(clippy::expect_used, reason = "hardcoded default keybinding strings are guaranteed valid")]
        Keybinding::parse(default).expect("default keybinding must parse")
    })
}
```

- [ ] **Step 5: Refactor `translate_layout_shortcut` to use `Bindings`**

Change signature to accept bindings:

```rust
pub fn translate_key_action(event: &KeyEvent, modifiers: ModifiersState, bindings: &Bindings) -> Option<KeyAction> {
    if event.state != ElementState::Pressed {
        return None;
    }

    if let Some(action) = translate_layout_shortcut(event, modifiers, bindings) {
        return Some(KeyAction::Layout(action));
    }

    // Settings keybinding (dynamic)
    if bindings.settings.matches(event, modifiers) {
        return Some(KeyAction::OpenSettings);
    }

    translate_key(event, modifiers).map(KeyAction::Terminal)
}

fn translate_layout_shortcut(event: &KeyEvent, modifiers: ModifiersState, bindings: &Bindings) -> Option<LayoutAction> {
    if bindings.split_vertical.matches(event, modifiers) {
        return Some(LayoutAction::SplitVertical);
    }
    if bindings.split_horizontal.matches(event, modifiers) {
        return Some(LayoutAction::SplitHorizontal);
    }
    if bindings.close_pane.matches(event, modifiers) {
        return Some(LayoutAction::ClosePane);
    }
    if bindings.cycle_pane.matches(event, modifiers) {
        return Some(LayoutAction::FocusNext);
    }
    None
}
```

- [ ] **Step 6: Store `Bindings` in `App` and pass to input handler**

In `crates/scribe-client/src/main.rs`, add `bindings: Bindings` to the `App` struct. Initialize in `App::new()`:

```rust
bindings: input::Bindings::parse(&config.keybindings),
```

Update all call sites of `translate_key_action` to pass `&self.bindings`.

- [ ] **Step 7: Verify build + lint**

Run: `cargo clippy --workspace`

- [ ] **Step 8: Commit**

```bash
git add crates/scribe-client/src/input.rs crates/scribe-client/src/main.rs
git commit -m "feat: implement configurable keybindings

Parse keybinding strings from config into modifier+key structs. Match
dynamically instead of hardcoded checks. Invalid bindings fall back to
defaults with a warning. Adding new actions requires only an enum
variant and config field — no parser changes."
```

---

### Task 7: Hot-reload consolidation

**Files:**
- Modify: `crates/scribe-client/src/main.rs:490-511`

- [ ] **Step 1: Expand `handle_config_changed`**

Replace the current `handle_config_changed` (which only handles theme) with the full version. Structure:

```rust
fn handle_config_changed(&mut self) {
    let new_config = match scribe_common::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config reload failed: {e}");
            return;
        }
    };

    let old = &self.config;

    // -- Theme --
    if old.appearance.theme != new_config.appearance.theme {
        let new_theme = resolve_theme(&new_config);
        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_theme(&new_theme);
        }
        self.theme = new_theme;
    }

    // -- Font params (single atlas rebuild for any change) --
    let font_changed = old.appearance.font != new_config.appearance.font
        || (old.appearance.font_size - new_config.appearance.font_size).abs() > f32::EPSILON
        || old.appearance.font_weight != new_config.appearance.font_weight
        || old.appearance.font_weight_bold != new_config.appearance.font_weight_bold
        || old.appearance.ligatures != new_config.appearance.ligatures
        || old.appearance.line_padding != new_config.appearance.line_padding;

    if font_changed {
        if let Some(gpu) = &mut self.gpu {
            let params = scribe_renderer::atlas::FontParams {
                family: new_config.appearance.font.clone(),
                size: new_config.appearance.font_size,
                weight: new_config.appearance.font_weight,
                weight_bold: new_config.appearance.font_weight_bold,
                ligatures: new_config.appearance.ligatures,
                line_padding: new_config.appearance.line_padding,
            };
            gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
            // TODO: send resize to server if grid size changed
        }
    }

    // -- Cursor --
    if let Some(gpu) = &mut self.gpu {
        gpu.renderer.set_cursor_shape(new_config.appearance.cursor_shape);
    }
    self.cursor_blink_enabled = new_config.appearance.cursor_blink;
    if !self.cursor_blink_enabled {
        self.cursor_visible = true;
    }

    // -- Opacity --
    if (old.appearance.opacity - new_config.appearance.opacity).abs() > f32::EPSILON {
        if !self.window_transparent && new_config.appearance.opacity < 1.0 {
            tracing::warn!("opacity < 1.0 requires restart to take effect (window was created without transparency)");
        } else {
            self.opacity = new_config.appearance.opacity;
        }
    }

    // -- Keybindings --
    self.bindings = input::Bindings::parse(&new_config.keybindings);

    self.config = new_config;

    tracing::info!("config hot-reloaded");
    self.request_redraw();
}
```

- [ ] **Step 2: Verify build + lint**

Run: `cargo clippy --workspace`

- [ ] **Step 3: Run E2E smoke test**

```bash
cargo build --release
docker build -f docker/Dockerfile.func -t scribe-test-func .
docker run --rm -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-func /tests/smoke.sh
```

Expected: Exit code 0. Check `test-output/result.log` for any regressions.

- [ ] **Step 4: Commit**

```bash
git add crates/scribe-client/src/main.rs
git commit -m "feat: consolidate hot-reload for all settings

handle_config_changed now handles font params (single atlas rebuild),
cursor shape/blink, opacity (with restart warning), and keybindings.
Theme reload was already working and is preserved."
```

---

## Verification Checklist

After all tasks are complete:

- [ ] `cargo check --workspace` — clean
- [ ] `cargo clippy --workspace` — clean
- [ ] `cargo fmt --all --check` — clean
- [ ] `cargo test --workspace` — all pass
- [ ] E2E smoke test (functional container) — pass
- [ ] E2E smoke test (visual container) — pass (cursor shape rendering)
- [ ] Manual spot-check: change `font`, `font_size`, `cursor_shape`, `opacity` in config.toml and verify hot-reload applies each
