# Rendering

The GPUI client paints terminal cells and bespoke chrome from immutable terminal snapshots.

## Terminal Renderer

The removed standalone renderer has been replaced by the GPUI paint path in
`crates/scribe-client`, which preserves terminal colour, fallback, ligature,
and box-drawing semantics without maintaining a separate wgpu pipeline.

### Ligature Detection

The renderer groups cells into same-styled runs via `detect_styled_runs` and shapes each run through cosmic-text to identify ligatures.

If a shaped glyph spans more than one terminal column or is a contextual alternate, it is treated as a ligature. Consecutive empty placeholder glyphs are merged with the following visual glyph to handle monospace font patterns.

#### Contextual Alternate Source Char

 reads the glyph's source character from `.source_char` rather than indexing the run's `chars` vec by `col_offset`.

`col_offset` counts wide characters as multiple grid columns while `chars` indexes them as one entry, so the two diverge after any wide character. Populating `source_char` from cosmic-text's `g.start..g.end` byte range during shaping keeps identity checks correct regardless of grid position — fixing the false-positive contextual-alternate detection that produced blank cells past emoji on the same run.

#### Tab Exclusion From Run Text

Tab characters are excluded from shaped run text entirely —  flushes the accumulator and skips the cell outright when it encounters `\t`, the same way it already skips wide-char spacer cells.

`unicode_width` gives `\t` a width of 0, so 's column-matching let a tab silently attach to the end of the preceding run instead of breaking it (e.g. a run's text became `"tests\t"`). Shaping that trailing tab through cosmic-text produced an arbitrary, oversized glyph advance, and since any shaped glyph spanning more than one column is inserted into the ligature map as-is (see above), the bogus span got inserted starting at the tab's own column and ran forward into the next word's real characters, silently replacing their rendered glyphs with slices of the tab's texture. This is the mechanism behind BSD `ls`'s columnar output (which separates entries with raw tabs, not spaces) dropping the first few characters of filenames when ligatures are enabled.  and  also treat `\t` as a blank cell (same as space and NUL) as defense in depth, independent of the run-detection fix. The fix lives entirely in `scribe-renderer`, which has no `cfg(target_os)` branches, so it applies identically on macOS and Linux.

### Cursor Rendering

Block cursor inverts foreground and background colours. Beam cursor renders the normal cell plus a thin vertical bar overlay. Underline cursor renders the normal cell plus a thin horizontal bar at the bottom.

### Color Space

All theme colours are specified in sRGB but the GPU pipeline operates in linear space. Conversion uses `srgb_to_linear_rgba` during theme loading.

The DIM flag is applied in sRGB space before conversion to match terminal convention. A dimming factor of 0.67 is used.

### Bold-Bright Colors

Cells with the BOLD flag have their foreground promoted to the bright palette variant via .

Basic ANSI colours 0-7 become indices 8-15, and the semantic `Foreground` becomes `BrightForeground` — a brighter variant computed by . RGB and already-bright colours pass through unchanged.

## Glyph Atlas

The  rasterizes glyphs via cosmic-text and caches them in a 1024x1024 RGBA8 texture.

### DPI Scaling

Font sizes and chrome dimensions are multiplied by `window.scale_factor()` so the UI renders at the native physical resolution.

The wgpu surface operates in physical pixels (e.g. 2x on Retina), so raw config values would appear at half the expected size without scaling. The client stores `scale_factor` on  and applies it to: font sizes in all four  construction sites (init, config hot-reload, zoom, resize), status bar height, tab bar height and padding, scrollbar width, content padding (via ), focus border width, and indicator height. On resize, scale-factor changes (e.g. dragging between monitors) are detected and the atlas is rebuilt.

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

 shapes an "M" at a given `weight` and records its advance;  stores both the regular and bold results. Ligature classification compares each shaped glyph against the width for its own weight:  divides a glyph's advance by the matching cell width to derive its column span, and  bounds a glyph's visual extent against it. Both take the glyph's  so the correct width is chosen. Measuring a legitimately wider bold glyph against the narrower regular-weight "M" previously made an ordinary single-cell bold glyph exceed the threshold and get misclassified as a multi-cell ligature, corrupting the ligature map and dropping the following cell's character.

### Font Fallbacks

Glyph shaping uses a Scribe-specific cosmic-text fallback list so terminal icon fonts win before generic symbol fonts.

 rebuilds the loaded system font database with . The fallback list prepends common Nerd Font symbol family names before the normal Unix symbol and emoji families. It also forbids `Unifont Sample`, because that font claims private-use codepoints and can render terminal icon glyphs as misleading sample symbols when no Nerd Font is installed.

If no icon font is installed, private-use icons still fall back to the primary font's missing-glyph box; Scribe does not synthesize platform logos.

### GPUI Fallback-Ordering Spike

The GPUI migration preserves this ordering by attaching the same list to each glyph run through `FontFallbacks`.

 demonstrates the pinned GPUI backend preserving `Symbols Nerd Font Mono` before `Unifont Sample` for U+E0B0. The decision and US3 impact are recorded in `specs/016-gpui-client-rebuild/spikes/nerd-font-fallback-ordering.md`.

### UV Computation

UV coordinates use float cell dimensions matching the GPU quad size, not ceiling-rounded canvas dimensions.

This ensures the GPU quad covers exactly the same number of texels as the shader has pixels, preventing texel skipping under nearest-filter sampling.

### Procedural Box Drawing

Box drawing (U+2500-U+257F) and block elements (U+2580-U+259F) are rendered procedurally via  instead of from the font.

This fills cells edge-to-edge with no font-bearing gaps. Output is white foreground on transparent background; the GPU fragment shader applies colours via `mix(bg, fg, alpha)`.

### Box Drawing Coverage

Line segments are decoded into four directional segments (up, down, left, right) with light, heavy, double, and dash weights.

Block elements use direct rectangle fills for halves, eighths, quarters, and shade characters with variable alpha.

### GPUI Box-Drawing Overlay

The GPUI client renders procedural box drawing through a paint-quad overlay rather than the text system, retaining edge-to-edge coverage regardless of font availability.

 selects U+2500–U+259F cells for the overlay. The GPUI port converts the same rasterizer alpha mask into foreground-coloured quads after cell backgrounds and before normal shaped text; see  for the shipped seam and `specs/016-gpui-client-rebuild/spikes/box-drawing-rendering.md` for the capability spike.

## Render Pipeline

The  is a wgpu render pipeline drawing instanced quads.

### Present Scheduling

Before presenting a rendered frame, the client calls 's `Window::pre_present_notify()` path so winit can schedule the next `RedrawRequested` against the actual presentation cadence.

When panes still have queued PTY output frames,  keeps the event loop in `ControlFlow::Poll` and requests another redraw so light bursts can keep animating while larger backlogs still catch up to the latest committed terminal state even if IPC user events keep arriving.

 delegates frame removal to , which first resolves the pane and only then pops its queue. A missing pane therefore cannot consume a queued frame.

### Bind Group

Three bindings: a uniform buffer (viewport size + cell size as two `vec2<f32>`, 16 bytes total, VERTEX stage), the glyph atlas texture (FRAGMENT stage, floating filterable), and a linear sampler (FRAGMENT stage, filtering).

### Instance Buffer

Dynamically sized with growth/shrinkage heuristics. Grows via doubling when count exceeds capacity; shrinks when usage drops below 25%.

A hash of the instance slice detects identical frames and skips GPU uploads. The hash is invalidated after atlas rebuilds to prevent stale UV reuse.

### Initial Capacity

The instance buffer starts at 10,000 entries and adjusts based on actual usage.

## Cell Instance

The GPU vertex data for a single cell, defined in .

### Fields

Each instance carries pixel position, size override, atlas UVs, foreground/background colours, corner radius, and alignment padding.

Specifically: pixel position (`[f32; 2]`), per-instance size override (`[f32; 2]`, zero means use uniform cell size), atlas UV min/max (`[f32; 2]` each), foreground and background colours (`[f32; 4]` each in linear RGBA), corner radius (`f32`), and alignment padding (`f32`). The struct derives `bytemuck::Pod` for direct GPU upload.

## Colour Palette

The  provides the xterm-256 colour lookup, converting all entries from sRGB to linear at construction time.

ANSI 0-15 are overridable by theme. The 6x6x6 colour cube (indices 16-231) uses intensity steps 0/95/135/175/215/255. The 24-step greyscale ramp (indices 232-255) spans values from 8 to 238 in steps of 10. Out-of-range named colours fall back to opaque magenta as an unmistakeable "missing colour" sentinel.

## GPUI Ported Rendering Logic

The GPUI client rebuild ports the renderer's pure colour and box-drawing logic into the `scribe-client` library crate so terminal output stays byte-for-byte identical across the cutover, independent of the wgpu pipeline.

These modules are display-independent: they own no GPU resources and are exercised by byte/colour-exact unit tests that lock the legacy renderer's output.  is the consumer that puts them on the live paint call.

### GPUI Colour Palette

 is a verbatim port of the xterm-256 palette, resolving the shared `vte::ansi::Color` values the Zed alacritty fork produces.

It reproduces the standard/bright ANSI entries, the 6×6×6 colour cube, and the greyscale ramp, all linearised at construction, with theme override of entries 0-15 and the opaque-magenta sentinel for out-of-table named colours.

### GPUI Colour Semantics

 holds the theme-derived default colours plus the palette and resolves a cell's raw fg/bg fields to linear RGBA, mirroring the legacy `resolve_cell_colors_raw`.

It applies bold→bright promotion via , the DIM 0.67 sRGB round-trip via , the `BrightForeground` boost via , and INVERSE/HIDDEN handling. Theme colours are linearised through .

GPUI's `Rgba` is already sRGB, so the paint path calls , which runs the identical rules and converts the result back with . Keeping one resolver and converting at the boundary is deliberate: duplicating the bold-bright / INVERSE / HIDDEN / DIM ordering in a second colour space is exactly how the two clients would drift.

### GPUI Box-Drawing Rasterizer

 ports the procedural rasterizer that emits a cell-sized RGBA alpha mask for U+2500–U+259F;  selects those codepoints.

Per the  capability spike, `TerminalElement` paints this mask as a foreground-coloured quad overlay after cell backgrounds and before shaped text, keeping edge-to-edge coverage regardless of font availability.

GPUI cannot upload a per-cell RGBA texture the way the wgpu atlas did, so  reduces the mask to the smallest set of uniform-alpha  rectangles that reproduces it exactly — horizontal runs first, then merged vertically. That reduction is what makes the overlay affordable: a full block becomes one quad instead of one per scanline, so a screen of box drawing costs a handful of quads per cell rather than hundreds.

### GPUI Window Opacity

`appearance.opacity` is a pure repaint in the GPUI client: the native surface is always alpha-capable and the configured value only scales the alpha of the backgrounds Scribe paints into it. See  for the reload seam.

 owns the derivation.  saturates out-of-range values into `0.0..=1.0` and maps NaN to fully opaque, so a malformed config degrades to a normal window instead of an invisible one — the config file itself is never validated on load, so every consumer clamps.  and  fold that value into a background's alpha, while  passes foreground colours through untouched. That split mirrors the legacy renderer's , which scaled each cell's background alpha and never its glyphs, so text stays readable over whatever the desktop shows through.

Two rules make the result equal the configured number rather than an accumulation of it. First, the window is opened with `WindowBackgroundAppearance::Transparent` unconditionally, even at opacity 1.0: surface capability is fixed at creation, so deriving it from the startup value would force a restart to ever go translucent — the legacy client's `window_transparent` flag had exactly that wart and refused live changes (). At 1.0 every painted background is alpha 1.0 and the window is pixel-identical to an opaque one. Second, the root element paints nothing at all. The titlebar, terminal grid and status bands tile the window edge to edge, so each pixel carries the opacity alpha exactly once; filling the root as well would composite a translucent band over a translucent root and land at 0.98 for a configured 0.85.

The alpha-aware surfaces are the terminal grid (), the titlebar and tab bar (), the prompt bar (), the terminal-status strip, and the window status bar (). Their colours come from the resolved theme rather than the literals the spike hardcoded, so a `theme` edit now repaints the grid and the strip too.

## GPUI Cell-Accurate Paint Path

The GPUI terminal grid resolves every visible property of a cell — colour, attributes, glyph coverage, and shaping — on the live paint call, so the ported rendering logic above reaches real pixels instead of only unit tests.

`Content` carries a `Cell` per grid position (character, raw `vte::ansi::Color` foreground and background, and the alacritty `Flags` bitset) rather than a `String` per row. The colours stay unresolved in the snapshot on purpose: a theme edit then repaints existing scrollback without re-running the parser, because `TerminalElement` resolves against the current theme every frame.

`TerminalElement::paint` lowers that snapshot onto one `gpui::canvas` rather than a div per row, because the three passes below must land in one paint call in order. A styled-div tree can express none of them.

### Paint Order

Each row paints cell backgrounds, then the box-drawing overlay, then shaped glyph runs — the same order the legacy wgpu renderer used.

Backgrounds come first so the overlay and the glyphs sit on top of them. Adjacent cells resolving to the same colour merge into one quad, and a cell whose resolved background equals the theme default paints nothing at all, so the window's own (possibly translucent) fill shows through instead of being painted over — that is what keeps  correct per cell. Only backgrounds are scaled by `appearance.opacity`; glyphs never are.

Box-drawing cells are then overlaid from  and replaced by a space in the shaped text, so the overlay is the only thing that draws them. The quads are rasterized in integer mask pixels and scaled onto the cell's exact fractional rect, which is what makes a stroke land precisely on the neighbouring cell's edge at any font size instead of leaving the rounding seam a whole-pixel mask would.

### Shell Cursor

The focused GPUI pane paints the terminal shell cursor immediately, then alternates it on a 530 ms cadence when `appearance.cursor_blink` is enabled.

[[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#viewport_shell_cursor]] projects the parser cursor into the immutable viewport snapshot. DECTCEM, vi mode, and ordinary scrollback suppress it; split-scroll keeps it on the pinned live tail.

[[crates/scribe-client/src/terminal_element.rs#TerminalElement#painted_cursor]] combines that snapshot with window focus, blink phase, and `appearance.cursor_shape`. DECSCUSR beam and underline requests win over the configured fallback; only the focused pane receives cursor paint.

[[crates/scribe-client/src/main.rs#TerminalView#tick_cursor_blink]] invalidates the view at each blink edge. Focus gain, keyboard activity, and config reload restart a visible phase, while an unfocused window paints no shell cursor and schedules no blink transitions.

### Glyph Runs

A whole row is shaped as one `shape_line` call with a `TextRun` per style change, and `force_width` set to the cell advance.

Shaping the row rather than the cell is what allows a contextual ligature to form across cells; `force_width` then pins every advancing glyph to the next grid column, so the ligature keeps its multi-cell outline while later cells stay on the grid. Adjacent cells with identical style merge into one run, which matters for correctness and not just cost: shaping only forms a ligature within a single run.

Each run carries `FontWeight` from `appearance.font_weight` / `font_weight_bold` (BOLD selects the bold weight), `FontStyle::Italic` for ITALIC, and underline / strikethrough decorations for the `ALL_UNDERLINES` and `STRIKEOUT` flags. Control characters are blanked before shaping, since `shape_line` rejects a newline outright.

### Font Fallbacks

Every run carries an explicit ordered fallback chain, Nerd Font symbol families first, so GPUI's platform text system cannot substitute its own ordering.

The list mirrors the legacy cosmic-text atlas (`SCRIBE_COMMON_FALLBACKS` in `crates/scribe-renderer/src/atlas.rs`): `Symbols Nerd Font Mono`, `Symbols Nerd Font`, `Nerd Font Symbols Mono`, `Nerd Font Symbols`, then the generic sans / mono / symbol / emoji families. `Unifont Sample` is deliberately excluded — its private-use mappings turn an unavailable icon into an unrelated sample glyph, which is worse than a visible tofu box. `specs/016-gpui-client-rebuild/spikes/nerd-font-fallback-ordering.md` records the capability spike.

#### Embedded Symbols Font Defeats GPUI Face Eviction

Carrying the chain is necessary but not sufficient on this GPUI revision: a stock symbols-only family can never enter it, so the client embeds a patched `Symbols Nerd Font Mono` that can.

`CosmicTextSystem::load_family` (gpui rev `f96212f`, `crates/gpui_wgpu/src/cosmic_text_system.rs`) drops any face whose charmap has no `'m'` glyph and calls `db_mut().remove_face` on it. Every stock `Symbols Nerd Font*` face fails that test, so each chain entry resolves to nothing and the face is evicted from the font database outright. Omitting the families does not help either: GPUI builds its `FontSystem` with cosmic-text's default `PlatformFallback` and exposes no equivalent of the legacy  `forbidden_fallback`, so automatic fallback picks `Unifont Sample` — the exact font the legacy renderer bans.

 therefore registers  — the upstream binary with a `U+006D` cmap alias added by `tools/patch-nerd-symbols-font.py` — with GPUI's text system before the first frame is shaped. The face passes the `'m'` check, keeps its upstream family name so the chain resolves it, and covers the icon ranges even on hosts with no Nerd Fonts installed. The alias never leaks into visible text: the chain is only consulted for codepoints the primary font lacks, and every terminal font covers `m`.

Live capture on the real client confirms the chain is live: `U+F09B` and `U+F121` — absent from the primary JetBrains Mono — render as the octocat and code icons from the embedded face instead of `Unifont Sample` hex boxes, alongside the `U+E0B0`/`U+E0B2`/`U+E0A0` powerline glyphs.

### Ligature Toggle

`appearance.ligatures` selects the OpenType features the runs are shaped with: `FontFeatures::disable_ligatures()` (`calt` off) when false, the font's own defaults when true.

The setting is read by  on the live config-load path, and `font_params_changed` already counts it as a font metric, so saving the edit repaints the grid without a restart — the same reload seam as `font` and `font_size`. `specs/016-gpui-client-rebuild/ligatures-spike.md` records the capability spike.

## Chrome Rendering

UI chrome (tab bars, status bars, dividers, dialogs) is rendered as solid or rounded quads via .

These produce `CellInstance` objects with zero UV coordinates (transparent-black atlas pixel) so the shader shows only the background colour. Rounded quads set a non-zero `corner_radius` for the shader's SDF rounding.

The IME preedit overlay piggybacks on this same chrome path:  emits a background-fill quad plus glyph cells plus a 1px theme-foreground underline into the chrome instance buffer above the terminal grid and below search / dialog overlays — no new wgpu pipeline or shader. See  for the full state machine and activation gate.
