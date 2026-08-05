# Rendering

The GPUI client paints terminal cells and bespoke chrome from immutable terminal snapshots.

## Terminal Renderer

`scribe-client` supplies immutable cell snapshots and terminal-specific paint
instructions; GPUI owns text shaping, scene composition, GPU submission, and
presentation.

[[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint]] creates
one GPUI canvas for the grid. Its paint callback resolves cell colours,
emits background and box-drawing quads, and supplies styled row text to GPUI in
that order. The client owns these terminal semantics but no renderer backend.

## Glyph Atlas

GPUI owns glyph shaping caches, rasterization, atlas allocation, and texture
uploads; `scribe-client` neither allocates nor addresses a glyph texture.

[[crates/scribe-client/src/terminal_element.rs#paint_row_text]] supplies a
logical text line, `TextRun` styles, font size, and forced cell width to
GPUI's `WindowTextSystem::shape_line`, then asks the returned `ShapedLine` to
paint. The pinned GPUI revision caches raster bounds in `TextSystem` and owns
the platform atlas behind that call.

### DPI Scaling

GPUI owns DPI scaling. The client supplies logical-pixel sizes only and never multiplies them by a scale factor itself.

`Window::paint_glyph` in the pinned gpui rev takes a logical `origin` and `font_size`, reads `self.scale_factor()`, scales the origin, and forwards the factor in `RenderGlyphParams` so rasterization happens at device resolution. Layout does the same via `stretch_auto_size_to_fill`. Nothing the client passes in is pre-scaled.

Every size the client hands to GPUI is therefore a logical pixel: `px(...)` values, the Tailwind-scale text helpers, and the configured terminal font size. [[crates/scribe-client/src/terminal_element.rs#GridFont#from_appearance]] clamps `appearance.font_size` to `MIN_FONT_SIZE` and passes it through unscaled, and the grid's `shape_line` calls in [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_preedit]] and [[crates/scribe-client/src/terminal_element.rs#paint_row_text]] hand that same `px(font.size)` straight to GPUI's text system. `scribe-client` contains no `set_rem_size` and no `text_size(` call, so GPUI's default `px(16.)` rem size is the only rem input to sizing.

The sole remaining app-level use of a scale factor is geometry replay: [[crates/scribe-client/src/restore_replay.rs#effective_padding]] multiplies configured content padding by `scale_factor` when reconstructing a pane's grid from a saved layout. That path scales rectangles, not fonts or chrome typography.

## Terminal Image Resources

Terminal image definitions own one bounded window-local GPUI source per image generation; placements and crops never duplicate uploads.

The pinned GPUI revision `f96212f2c50f54d93712fa130d6226b1ce7d76b5`
keys `Window::paint_image` atlas entries by `RenderImage` identity and frame.
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache#get_or_insert_for_session]]
therefore caches `(session_id, image_id, generation) -> Arc<RenderImage>` for
the whole view, charges two canonical byte lengths for texture plus upload
staging, and removes only closed-session or unplaced entries before any image
primitive is queued. When live sources fill the frozen per-view projected-GPU
ceiling, admission rejects the new source for that frame instead of evicting a
tile that earlier primitives may still reference.

Admission is also the only moment the view's projected charge rises, and the
only observable moment of a first upload, so it logs the definition's identity
and dimensions, the entry's charge, the running total, and the cached source
count. Resource review reads its GPU numbers from that line rather than
re-deriving them from definitions; the line carries no pixels.

GPUI exposes no source UV rectangle at this revision. Instead,
[[crates/scribe-client/src/gpui_image_lifecycle.rs#paint_cropped_image]] scales
the full image so the requested source rectangle covers its destination,
translates by the source offset, and intersects it with
`Window::with_content_mask`. The WGPU shader derives UVs from those translated
full bounds, so every crop shares the original atlas key without a crop cache
or GPUI patch.

Cleanup remains explicit. Final cache removal calls `Window::drop_image` before
releasing the last source reference. GPUI removes every frame key and WGPU
deallocates its atlas tile. Device recovery clears GPUI's atlas but preserves
the CPU `RenderImage`; the next paint reconstructs the same key lazily.
Definition deletion and pane/session removal are reconciled during paint by
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache#retain_session_definitions]]
and
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache#retain_sessions]],
so both paths drop atlas keys while a live `Window` is available.
[[terminal-images#GPUI Lifecycle Verification]] records Linux runtime evidence
and the separate native Metal gate.

## Terminal Image Paint Phases

Terminal images use six ordered paint phases around cell content, keeping Kitty z-order and Sixel chronology compatible with terminal text.

[[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_grid]] paints
deep Kitty placements and Sixel first, then non-default cell backgrounds,
remaining negative Kitty placements, box drawing and shaped text, nonnegative
Kitty placements, and selection/find/cursor/split-scroll/scrollbar/chrome
overlays. Selection repaints above images, then restores overlapping resolved
find cells so the search accent keeps its existing precedence.
Placements sort by z-index, image id, placement id, then committed chronology;
Sixel retains completion order in its background raster band.

Classic placements derive destination bounds from current logical cell metrics,
then [[crates/scribe-client/src/gpui_image_lifecycle.rs#paint_cropped_image_clipped]]
keeps full placement scaling while intersecting its content mask with the pane
viewport. Typed scroll and resize effects preserve source crop, destination
extent, and pixel offsets while moving/intersecting an exclusive logical-cell
clip carried by the common placement. Each renderer converts that clip with
its current cell metrics, preventing repeated-scroll rounding drift and
preserving offset fractions across resize. Scribe never pre-multiplies
placement coordinates by GPUI's DPI scale. Placeholder prototypes carry no
physical clip; their matching terminal cells remain authoritative.

Unicode-placeholder cells preserve their three zero-width coordinate marks and
underline colour in [[crates/scribe-client/src/terminal.rs#Cell]].
[[crates/scribe-client/src/kitty_placeholder.rs#kitty_placeholder_diacritic_index]]
maps the official 297 marks, and
[[crates/scribe-client/src/terminal_element.rs#paint_placeholder_cells]] resolves
8/24/32-bit image identity, deterministic missing placement identity, and
left-cell inheritance. It aspect-fits the source once across the virtual
placement, then clips that destination through each matching cell, preserving
transparent cell backgrounds and source aspect. Placeholder markers remain
absent from the shaped glyph pass; the reserved IPC background byte adds no
opacity.

## Render Pipeline

GPUI owns frame scheduling, scene batching, render pipelines, command
submission, and presentation; `scribe-client` contributes element and canvas
primitives rather than GPU resources.

[[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_grid]]
lowers each terminal snapshot into GPUI `Window::paint_quad` calls and shaped
text. GPUI records those primitives in its scene and selects the platform
renderer. The client has no bind group, instance buffer, or cell vertex type.

The direct `wgpu` call in [[crates/scribe-client/src/main.rs#probe_vulkan]] is
an installer preflight that opens no window and draws no frame. It is separate
from the GPUI-owned render path.

## GPUI Ported Rendering Logic

`scribe-client` owns display-independent terminal colour and box-drawing logic,
while GPUI owns every GPU resource used to put that logic on screen.

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

`CosmicTextSystem::load_family` (gpui rev `f96212f`, `crates/gpui_wgpu/src/cosmic_text_system.rs`) drops any face whose charmap has no `'m'` glyph and calls `db_mut().remove_face` on it. Every stock `Symbols Nerd Font*` face fails that test, so each chain entry resolves to nothing and the face is evicted from the font database outright. Omitting the families does not help either: GPUI builds its `FontSystem` with cosmic-text's default `PlatformFallback` and exposes no equivalent of the legacy  `forbidden_fallback`, so automatic fallback picks `Unifont Sample` — the exact font the legacy renderer banned.

 therefore registers  — the upstream binary with a `U+006D` cmap alias added by `tools/patch-nerd-symbols-font.py` — with GPUI's text system before the first frame is shaped. The face passes the `'m'` check, keeps its upstream family name so the chain resolves it, and covers the icon ranges even on hosts with no Nerd Fonts installed. The alias never leaks into visible text: the chain is only consulted for codepoints the primary font lacks, and every terminal font covers `m`.

Live capture on the real client confirms the chain is live: `U+F09B` and `U+F121` — absent from the primary JetBrains Mono — render as the octocat and code icons from the embedded face instead of `Unifont Sample` hex boxes, alongside the `U+E0B0`/`U+E0B2`/`U+E0A0` powerline glyphs.

### Ligature Toggle

`appearance.ligatures` selects the OpenType features the runs are shaped with: `FontFeatures::disable_ligatures()` (`calt` off) when false, the font's own defaults when true.

The setting is read by  on the live config-load path, and `font_params_changed` already counts it as a font metric, so saving the edit repaints the grid without a restart — the same reload seam as `font` and `font_size`. `specs/016-gpui-client-rebuild/ligatures-spike.md` records the capability spike.

## Chrome Rendering

GPUI owns chrome layout, clipping, scene batching, and presentation;
`scribe-client` supplies theme-derived element styles and terminal-specific
canvas primitives.

[[crates/scribe-client/src/main.rs#TerminalView#render]] builds the window shell
from GPUI elements for title bars, panes, dividers, prompts, status bands, and
dialogs. [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_preedit]]
adds the IME backdrop, shaped text, and underline through the same GPUI canvas;
it does not allocate a separate pipeline or shader.
