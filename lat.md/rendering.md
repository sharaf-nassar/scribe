# Rendering

The GPU rendering pipeline draws terminal content and UI chrome in a single instanced draw call per frame using wgpu.

## Terminal Renderer

The [[crates/scribe-renderer/src/lib.rs#TerminalRenderer]] orchestrates the glyph atlas, colour palette, and render pipeline. It collects cells from the alacritty_terminal display iterator, detects ligatures via a styled-run pre-pass, and builds a flat array of [[rendering#Cell Instance]] for GPU submission.

### Ligature Detection

The renderer groups cells into same-styled runs via `detect_styled_runs` and shapes each run through cosmic-text to identify ligatures.

If a shaped glyph spans more than one terminal column or is a contextual alternate, it is treated as a ligature. Consecutive empty placeholder glyphs are merged with the following visual glyph to handle monospace font patterns.

#### Contextual Alternate Source Char

[[crates/scribe-renderer/src/lib.rs#is_contextual_alternate]] reads the glyph's source character from [[crates/scribe-renderer/src/atlas.rs#ShapedRunGlyph]]`.source_char` rather than indexing the run's `chars` vec by `col_offset`.

`col_offset` counts wide characters as multiple grid columns while `chars` indexes them as one entry, so the two diverge after any wide character. Populating `source_char` from cosmic-text's `g.start..g.end` byte range during shaping keeps identity checks correct regardless of grid position — fixing the false-positive contextual-alternate detection that produced blank cells past emoji on the same run.

### Cursor Rendering

Block cursor inverts foreground and background colours. Beam cursor renders the normal cell plus a thin vertical bar overlay. Underline cursor renders the normal cell plus a thin horizontal bar at the bottom.

### Color Space

All theme colours are specified in sRGB but the GPU pipeline operates in linear space. Conversion uses `srgb_to_linear_rgba` during theme loading.

The DIM flag is applied in sRGB space before conversion to match terminal convention. A dimming factor of 0.67 is used.

### Bold-Bright Colors

Cells with the BOLD flag have their foreground promoted to the bright palette variant via [[crates/scribe-renderer/src/lib.rs#bold_to_bright]].

Basic ANSI colours 0-7 become indices 8-15, and the semantic `Foreground` becomes `BrightForeground` — a brighter variant computed by [[crates/scribe-renderer/src/lib.rs#boost_srgb_brightness]]. RGB and already-bright colours pass through unchanged.

## Glyph Atlas

The [[crates/scribe-renderer/src/atlas.rs#GlyphAtlas]] rasterizes glyphs via cosmic-text and caches them in a 1024x1024 RGBA8 texture.

### DPI Scaling

Font sizes and chrome dimensions are multiplied by `window.scale_factor()` so the UI renders at the native physical resolution.

The wgpu surface operates in physical pixels (e.g. 2x on Retina), so raw config values would appear at half the expected size without scaling. The client stores `scale_factor` on [[crates/scribe-client/src/main.rs#App]] and applies it to: font sizes in all four [[crates/scribe-renderer/src/atlas.rs#FontParams]] construction sites (init, config hot-reload, zoom, resize), status bar height, tab bar height and padding, scrollbar width, content padding (via [[crates/scribe-client/src/pane.rs#effective_padding]]), focus border width, and indicator height. On resize, scale-factor changes (e.g. dragging between monitors) are detected and the atlas is rebuilt.

### Shelf Packing

The `ShelfPacker` places glyphs using a simple shelf algorithm: advance along the current row until full, then start a new shelf.

The packer starts at (1,1) to reserve a transparent-black pixel at (0,0) for empty cells. One pixel of padding between entries prevents atlas bleeding under bilinear filtering.

### Cache Management

The shaped glyph cache is capped at 8192 entries; the run shape cache at 4096. Both use the same eviction strategy.

When exceeded, roughly half the entries are evicted using an alternating keep pattern to avoid unbounded growth without a burst of misses after a full clear.

### Rasterization

Characters are shaped with cosmic-text and rasterized via the swash cache, then blitted onto a cell-sized canvas and uploaded to the atlas.

Advanced shaping is used for ligatures, Basic when disabled. Mask images are expanded to RGBA by filling white; Color images are kept as-is. Swash placement offsets position the glyph on the canvas.

### UV Computation

UV coordinates use float cell dimensions matching the GPU quad size, not ceiling-rounded canvas dimensions.

This ensures the GPU quad covers exactly the same number of texels as the shader has pixels, preventing texel skipping under nearest-filter sampling.

### Procedural Box Drawing

Box drawing (U+2500-U+257F) and block elements (U+2580-U+259F) are rendered procedurally via [[crates/scribe-renderer/src/box_drawing.rs#render]] instead of from the font.

This fills cells edge-to-edge with no font-bearing gaps. Output is white foreground on transparent background; the GPU fragment shader applies colours via `mix(bg, fg, alpha)`.

### Box Drawing Coverage

Line segments are decoded into four directional segments (up, down, left, right) with light, heavy, double, and dash weights.

Block elements use direct rectangle fills for halves, eighths, quarters, and shade characters with variable alpha.

## Render Pipeline

The [[crates/scribe-renderer/src/pipeline.rs#TerminalPipeline]] is a wgpu render pipeline drawing instanced quads.

### Present Scheduling

Before presenting a rendered frame, the client calls [[crates/scribe-client/src/main.rs#App#handle_redraw]]'s `Window::pre_present_notify()` path so winit can schedule the next `RedrawRequested` against the actual presentation cadence.

When panes still have queued PTY output frames, [[crates/scribe-client/src/main.rs#App#about_to_wait]] keeps the event loop in `ControlFlow::Poll` and requests another redraw so light bursts can keep animating while larger backlogs still catch up to the latest committed terminal state even if IPC user events keep arriving.

### Bind Group

Three bindings: a uniform buffer (viewport size + cell size as two `vec2<f32>`, 16 bytes total, VERTEX stage), the glyph atlas texture (FRAGMENT stage, floating filterable), and a linear sampler (FRAGMENT stage, filtering).

### Instance Buffer

Dynamically sized with growth/shrinkage heuristics. Grows via doubling when count exceeds capacity; shrinks when usage drops below 25%.

A hash of the instance slice detects identical frames and skips GPU uploads. The hash is invalidated after atlas rebuilds to prevent stale UV reuse.

### Initial Capacity

The instance buffer starts at 10,000 entries and adjusts based on actual usage.

## Cell Instance

The GPU vertex data for a single cell, defined in [[crates/scribe-renderer/src/types.rs#CellInstance]].

### Fields

Each instance carries pixel position, size override, atlas UVs, foreground/background colours, corner radius, and alignment padding.

Specifically: pixel position (`[f32; 2]`), per-instance size override (`[f32; 2]`, zero means use uniform cell size), atlas UV min/max (`[f32; 2]` each), foreground and background colours (`[f32; 4]` each in linear RGBA), corner radius (`f32`), and alignment padding (`f32`). The struct derives `bytemuck::Pod` for direct GPU upload.

## Colour Palette

The [[crates/scribe-renderer/src/palette.rs#ColorPalette]] provides the xterm-256 colour lookup, converting all entries from sRGB to linear at construction time.

ANSI 0-15 are overridable by theme. The 6x6x6 colour cube (indices 16-231) uses intensity steps 0/95/135/175/215/255. The 24-step greyscale ramp (indices 232-255) spans values from 8 to 238 in steps of 10. Out-of-range named colours fall back to opaque magenta as an unmistakeable "missing colour" sentinel.

## Chrome Rendering

UI chrome (tab bars, status bars, dividers, dialogs) is rendered as solid or rounded quads via [[crates/scribe-renderer/src/chrome.rs#solid_quad]].

These produce `CellInstance` objects with zero UV coordinates (transparent-black atlas pixel) so the shader shows only the background colour. Rounded quads set a non-zero `corner_radius` for the shader's SDF rounding.
