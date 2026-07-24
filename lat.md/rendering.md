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

#### Tab Exclusion From Run Text

Tab characters are excluded from shaped run text entirely — [[crates/scribe-renderer/src/lib.rs#detect_styled_runs]] flushes the accumulator and skips the cell outright when it encounters `\t`, the same way it already skips wide-char spacer cells.

`unicode_width` gives `\t` a width of 0, so [[crates/scribe-renderer/src/lib.rs#RunAccum#matches]]'s column-matching let a tab silently attach to the end of the preceding run instead of breaking it (e.g. a run's text became `"tests\t"`). Shaping that trailing tab through cosmic-text produced an arbitrary, oversized glyph advance, and since any shaped glyph spanning more than one column is inserted into the ligature map as-is (see above), the bogus span got inserted starting at the tab's own column and ran forward into the next word's real characters, silently replacing their rendered glyphs with slices of the tab's texture. This is the mechanism behind BSD `ls`'s columnar output (which separates entries with raw tabs, not spaces) dropping the first few characters of filenames when ligatures are enabled. [[crates/scribe-renderer/src/lib.rs#TerminalRenderer#resolve_glyph_uv_raw]] and [[crates/scribe-renderer/src/lib.rs#TerminalRenderer#resolve_glyph_uv_for_collected_fields]] also treat `\t` as a blank cell (same as space and NUL) as defense in depth, independent of the run-detection fix. The fix lives entirely in `scribe-renderer`, which has no `cfg(target_os)` branches, so it applies identically on macOS and Linux.

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

### Weight-Aware Cell Measurement

Bold glyphs are shaped at a heavier weight than regular text, so the atlas measures a separate reference cell width per weight (`cell_size` and `bold_cell_size`) for ligature classification.

[[crates/scribe-renderer/src/atlas.rs#measure_cell]] shapes an "M" at a given `weight` and records its advance; [[crates/scribe-renderer/src/atlas.rs#GlyphAtlas]] stores both the regular and bold results. Ligature classification compares each shaped glyph against the width for its own weight: [[crates/scribe-renderer/src/atlas.rs#GlyphAtlas#shape_run_uncached]] divides a glyph's advance by the matching cell width to derive its column span, and [[crates/scribe-renderer/src/atlas.rs#GlyphAtlas#fits_single_cell]] bounds a glyph's visual extent against it. Both take the glyph's [[crates/scribe-renderer/src/atlas.rs#GlyphStyle]] so the correct width is chosen. Measuring a legitimately wider bold glyph against the narrower regular-weight "M" previously made an ordinary single-cell bold glyph exceed the threshold and get misclassified as a multi-cell ligature, corrupting the ligature map and dropping the following cell's character.

### Font Fallbacks

Glyph shaping uses a Scribe-specific cosmic-text fallback list so terminal icon fonts win before generic symbol fonts.

[[crates/scribe-renderer/src/atlas.rs#scribe_font_system]] rebuilds the loaded system font database with [[crates/scribe-renderer/src/atlas.rs#ScribeFontFallback]]. The fallback list prepends common Nerd Font symbol family names before the normal Unix symbol and emoji families. It also forbids `Unifont Sample`, because that font claims private-use codepoints and can render terminal icon glyphs as misleading sample symbols when no Nerd Font is installed.

If no icon font is installed, private-use icons still fall back to the primary font's missing-glyph box; Scribe does not synthesize platform logos.

### GPUI Fallback-Ordering Spike

The GPUI migration preserves this ordering by attaching the same list to each glyph run through `FontFallbacks`.

[[tools/gpui-font-fallback-spike/src/main.rs#verify_nerd_font_precedes_generic_symbols]] demonstrates the pinned GPUI backend preserving `Symbols Nerd Font Mono` before `Unifont Sample` for U+E0B0. The decision and US3 impact are recorded in `specs/016-gpui-client-rebuild/spikes/nerd-font-fallback-ordering.md`.

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

[[crates/scribe-client/src/main.rs#App#drain_pane_output_until_frame]] delegates frame removal to [[crates/scribe-client/src/main.rs#App#apply_next_pane_output_frame]], which first resolves the pane and only then pops its queue. A missing pane therefore cannot consume a queued frame.

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

The IME preedit overlay piggybacks on this same chrome path: [[crates/scribe-client/src/main.rs#App#apply_preedit_overlay]] emits a background-fill quad plus glyph cells plus a 1px theme-foreground underline into the chrome instance buffer above the terminal grid and below search / dialog overlays — no new wgpu pipeline or shader. See [[client#Input#IME Composition]] for the full state machine and activation gate.
