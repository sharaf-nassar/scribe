# Rendering

The GPUI client paints terminal cells and bespoke chrome from immutable terminal snapshots.

## Terminal Renderer

`scribe-client` supplies immutable cell snapshots and terminal-specific paint
instructions; GPUI owns text shaping, scene composition, GPU submission, and
presentation.

[[crates/scribe-client/src/terminal_element.rs#PaneContentView]] retains each pane's base canvas. Changed rows resolve colors and shape text once; unchanged base views reuse GPUI paint primitives. A separate overlay canvas paints selection, cursor and transient chrome without preparing terminal rows.

## Glyph Atlas

GPUI owns glyph shaping caches, rasterization, atlas allocation, and texture
uploads; `scribe-client` neither allocates nor addresses a glyph texture.

[[crates/scribe-client/src/terminal_element.rs#shape_row_text]] supplies a
logical text line, `TextRun` styles, font size, and forced cell width to
GPUI's `WindowTextSystem::shape_line`, then retains the returned `ShapedLine` for changed-base paint calls. Overlay glyphs use the same shaping helper. The pinned GPUI revision caches raster bounds in `TextSystem` and owns
the platform atlas behind that call.

### DPI Scaling

GPUI owns DPI scaling. The client supplies logical-pixel sizes only and never multiplies them by a scale factor itself.

`Window::paint_glyph` in the pinned gpui rev takes a logical `origin` and `font_size`, reads `self.scale_factor()`, scales the origin, and forwards the factor in `RenderGlyphParams` so rasterization happens at device resolution. Layout does the same via `stretch_auto_size_to_fill`. Nothing the client passes in is pre-scaled.

Every size the client hands to GPUI is therefore a logical pixel: `px(...)` values, the Tailwind-scale text helpers, and the configured terminal font size. [[crates/scribe-client/src/terminal_element.rs#GridFont#from_appearance]] clamps `appearance.font_size` to `MIN_FONT_SIZE` and passes it through unscaled, and the grid's `shape_line` calls in [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_preedit]] and [[crates/scribe-client/src/terminal_element.rs#paint_row_text]] hand that same `px(font.size)` straight to GPUI's text system. `scribe-client` contains no `set_rem_size` and no `text_size(` call, so GPUI's default `px(16.)` rem size is the only rem input to sizing.

No app-level scale-factor multiplication remains. The last one was geometry replay, which scaled a configured content padding into a restored pane's grid; the GPUI client paints a pane edge to edge and has no content-padding setting, so [[crates/scribe-client/src/restore_replay.rs#grid_for_rect|grid_for_rect]] divides the painted rect by the cell box and nothing else.

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
[[terminal-images#Layered GPUI Renderer Verification]] records Linux runtime
evidence and the separate native Metal gate.

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

Accepted image `Scroll` effects mutate Alacritty's grid directly and bypass `Term` damage. Live commits and replay commits that drain these effects therefore force a viewport reread through [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#make_content_with_damage]]. Equal rows still retain identity; text and image publication remain atomic. This also covers accepted effects not currently emitted by the stock server.

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

## Retained GPU terminal bases

A Linux/WGPU path that retains terminal bases past GPUI scene replay and
upload, so a pane whose text has settled is recomposited from one texture
instead of re-issuing every glyph each frame.

[[crates/scribe-client/src/terminal_element.rs#retained_gpu_base_enabled]] reads
a process-wide flag the client publishes from `appearance.retained_gpu_bases`
at startup and on every config reload, so the setting reaches an installed
binary and can be turned off without a rebuild. `SCRIBE_DEV_RETAINED_GPU=0|1`
overrides the saved value for the A/B rigs, mirroring `SCRIBE_DISABLE_ANIMATIONS`;
it is read once per process, so a config reload cannot change meaning mid-run.
The flag defaults off in the renderer itself, so a frame painted before config
is applied keeps the ordinary path. Server lifecycle and animation policy are
untouched, and non-Linux clients do not compile the retained path at all.

The measured evidence is one NVIDIA/Vulkan machine: per frame it lowered both
CPU and GPU cost on every workload, but nothing has been run on AMD, Intel,
Wayland or macOS, and energy was never measured. That is why the key exists
rather than the behaviour being unconditional.

The disposable native-display border workload still used 84.12% of one CPU
core after preparation caching, with zero base/row preparations. Scene replay,
stable sorting and bounds insertion accounted for 15.45%, 13.93% and 12.42% of
the fresh profile. That is what motivated retention beyond the CPU preparation
cache, and the per-frame measurements above are what settled whether it paid.

Pinned `gpui` and `gpui_wgpu` are patched under `third_party/gpui-retained/`,
vendored complete except for upstream's `examples/`, `docs/`, `tests/` and
`benches/`, which nothing here compiles and which were most of the copied
bytes. Their `[[example]]`/`[[bench]]` manifest targets were removed with them.
Upstream's own `gpui` unit tests cannot run in this tree regardless, because
they `include_bytes!` fonts from Zed's repository-root `assets/`, so these two
packages are linted with `--lib` rather than `--all-targets`.
[[third_party/gpui-retained/gpui/src/scene.rs#RetainedLayer]] holds an immutable
captured scene behind one outer ordering reference. WGPU caches its texture by
view owner and capture revision. Hits composite without base scene uploads;
misses submit captures before main-frame buffer writes, but only after an owner
repeats its revision. First-seen/changing revisions paint directly without
waiting or suppressing frames. Revision history follows cache visibility and
invalidation; stale textures never composite. A capture that cannot fit the
instance buffer at its ceiling keeps its allocation with no revision, so the
next admitted capture reuses the texture instead of reallocating. A view paints
at most one retained layer per frame; debug builds assert it, since a second
layer would evict the first from the owner-keyed cache every frame.

Sprite draw order is keyed on the atlas *texture*, never the individual tile.
A tile id records where a glyph landed, which follows the order glyphs were
first rasterized, and that order varies between launches: identical sessions
allocated 86 shared glyphs at different origins, diverging by the ninth miss.
Sorting on it reordered equal-order sprites run to run, and blending overlapping
sprites in a different sequence left one-level rounding differences, so two
launches of one binary disagreed in 56-78 pixels and no exact-pixel oracle could
hold. Batching only needs sprites sharing a texture to be contiguous and the
sort is stable, so the texture index preserves batches while ties keep paint
order. Glyph rasterization was never the variable: every glyph sampled across
those launches had an identical bitmap checksum.

Repeating once is not proof that content rests: a pane producing output pauses
for a single frame often enough to earn a texture its next frame throws away,
which is why concurrent output recorded captures against zero hits. An owner
whose texture is replaced having never been composited therefore has to show
twice the stability before the next capture, doubling to a bounded ceiling,
and any reuse resets it. Repurposing an allocation for new content clears the
reuse flag, so the requirement cannot be escaped by holding the same texture. Cursor, selection, IME,
scrollbar, annotations, animated borders and outer background remain separate.

Capture bounds round outward in device pixels while shader calculations retain
window coordinates. RGBA8/BGRA8 captures store premultiplied pixels regardless
of window presentation alpha mode: either the existing shader or its SrcAlpha
blend factor supplies the multiplication. The composite uses factor One.
Images, ordinary paint-layer scopes and unsupported capture content retain
normal painting. Backend budget or format refusal draws the supported inner
scene directly. Each renderer admits at most 64 MiB of logical layer textures,
reuses equal sizes and prunes absent owners. Driver-held in-flight references
can temporarily exceed that logical budget. Private oversized-window checks
exercise single-layer and aggregate refusal, absent-owner pruning through
image exclusion, and subsequent readmission with pixel comparisons. They do
not establish physical-memory limits or an LRU eviction policy. Recovery must
invalidate retained textures and force fresh paint after atlas reset. A private
logical-device destruction check exercises the platform recovery path and
verifies fresh capture, resumed hits, preserved session identity and exact
before/after glyph pixels. It is not physical GPU reset, driver-crash or
multi-window recovery coverage. Private 1.25x steady-cursor and real IBus
preedit checks match direct rendering without recapturing the base; committing
text causes one new capture. These are overlay-parity checks, not blink-phase
or independent baseline cursor-geometry coverage.

Native owned-window RGBA parity passes on the measured NVIDIA/Vulkan X11
surface, but configured 0.95 opacity still produces background alpha 255 in
both disabled controls and the retained case. The surface advertises only
`Opaque` presentation, so this does not qualify alpha-preserving composition.
A later same-host comparison ran the pre-vendor client against the current one
under the same compositor: both produced alpha 255, so this is a surface
capability on this host rather than anything retention introduced, and
`scribe-5f2i` closed on that evidence. Forcing an unsupported presentation mode
remains invalid, and whole-window opacity is not a substitute because it would
fade glyphs too.

`GPUI_RETAINED_LAYER_PROBE` enables separate capture, actual hit, composite,
fallback, upload-span and logical texture-byte counters. Image exclusions and
rejected CPU captures are not backend fallback counts. These counters do not
prove completed presentation cadence or lower GPU utilization. Evidence covers
matched image/default paths, fractional-DPI lifecycle and overlays, logical
budget and device-loss recovery, native RGBA parity, cross-launch pixel
determinism, and the Linux regression, visual and build gates, which run
through this path now that it is on by default. What remains unproven is
hardware and duration: every number comes from short samples on one
NVIDIA/Vulkan host, with nothing measured on AMD, Intel, Wayland or macOS, no
energy measurement, and no physical GPU reset, driver crash or suspend/resume
coverage. That is the risk `appearance.retained_gpu_bases` exists to let a user
retire without a rebuild.
Provenance, scenario limits and the counter contract live in
`third_party/gpui-retained/README.md`.

Independent dev-only `SCRIBE_DEV_GPU_TIMESTAMPS=1` requests supported timestamp
queries. Bounded, separate main/capture readback pools collect asynchronously
without waiting for the GPU. Main intervals include continuation and path
passes; retained capture intervals are separate. Overflow aborts release their
slots. The probe reports elapsed nanoseconds, sample counts, skips and errors,
and splits main samples per renderer because process-global totals mix windows
of different sizes and frame shares. Renderer index follows window creation
order, so each renderer logs its index with its surface size.
These times exclude queue uploads and compositor work and are not utilization
percentages. Comparisons require coverage and an instrumentation-overhead
control. The private idle NVML probe returned no utilization values; active
native workloads returned intermittent numeric values, not complete coverage.

Reuse-gated native measurements show lower CPU cost for stable animated bases
and one updating pane. Per-second totals also showed higher concurrent-output
CPU and higher border GPU pass time, but those totals compare unequal delivered
work: retention sustains about 177-182 border frames/s against 126-137, because
the control is CPU-bound and drops frames. Three repeated runs with per-renderer
attribution compared cost per frame for the same window, and every retained
sample was below every control sample: medians 17.7% lower GPU per frame for
border animation, 29.7% for one updating pane and 13.9% for concurrent output,
with CPU per frame 58.1% and 46.4% lower for the first two. Concurrent-output
CPU per frame is 3.9% higher, the one measured per-frame regression, where
captures are attempted and never reused. Per-frame times drift between runs and
controls spread over 20% within a run, so only within-run sequential same-binary
comparisons are used. A temporary in-pass probe attributed 9.2-9.3% of main pass
time to retained batches and 98.8-98.9% to draw commands, excluding render-pass
clear and store boundaries as the cost; it has been removed. This is not
production acceptance, energy measurement, or cross-platform evidence. Fixture population and per-window geometry
must be checked independently: process-wide root and pane counters flush at
different rendering stages and their ratio cannot prove visible-pane count.

## GPUI Ported Rendering Logic

`scribe-client` owns display-independent terminal colour and box-drawing logic,
while GPUI owns every GPU resource used to put that logic on screen.

These modules are display-independent and own no GPU resources. Unit tests pin SGR semantics and exact sRGB values; [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_grid]] puts them on the live paint path.

### GPUI Colour Palette

[[crates/scribe-client/src/palette.rs#ColorPalette]] stores the xterm-256 palette in sRGB, the color space accepted by GPUI, and resolves the raw `vte::ansi::Color` values from the terminal snapshot.

Standard/bright ANSI, the 6×6×6 cube and greyscale entries are table lookups. Truecolor is byte-to-float normalization only. Theme overrides copy sRGB entries 0-15 including alpha; out-of-table named colors retain the opaque-magenta sentinel.

### GPUI Colour Semantics

[[crates/scribe-client/src/color.rs#TerminalColors]] resolves terminal colors entirely in sRGB. GPUI owns the rendering conversion, so no CPU transfer function runs per cell.

[[crates/scribe-client/src/color.rs#TerminalColors#resolve_cell_colors]] keeps one ordered rule set: BOLD promotion, color resolution, INVERSE swap, HIDDEN foreground replacement, then DIM. DIM multiplies the resulting foreground's RGB by 0.67 without changing alpha. Semantic bright and dim foregrounds are precomputed when the theme changes.

The old renderer required linear inputs; GPUI does not. Keeping that old storage domain after the port caused six inverse-transfer calls for a common foreground/background cell, plus forward conversions for truecolor and extra round trips for DIM. The native sRGB design deletes that work rather than caching it or duplicating SGR rules. Palette, cursor, selection and IME backgrounds all use the same domain. Existing `opaque_slot` and `scale_alpha` paint policies are unchanged.

The pinned [GPUI shader implementation](https://github.com/zed-industries/zed/blob/f96212f2c50f54d93712fa130d6226b1ce7d76b5/crates/gpui_wgpu/src/shaders.wgsl) owns backend color handling. This matches Scribe's existing sRGB chrome rather than adding a second renderer contract.

#### Repaint Optimization Boundaries

Pane base caching and row preparation reuse avoid repeated terminal work on unrelated root frames; GPU retention is not part of this refactor.

[[client#Client#GPUI Client Spike#Per-Pane Grids And Sizing#Pane-grid cached-view decision]] defines publication, cache keys, overlay separation, scheduling and hidden-session behavior. The pinned GPUI cache replays recorded primitives into scene ordering and sorting. WGPU clears the full target, so this is CPU-side preparation reuse, not partial GPU presentation.

With `SCRIBE_PERF_PROBE=/absolute/report-path`, the existing probe now reports `content_preparations` (actual base-view render calls), `row_preparations` (resolved/shaped base rows), `content_cache_reuses` (unchanged inputs eligible for a cached mount), and `terminal_overlay_paints`, alongside root `frames`. Bounds/refresh can still force a base render after a cached mount, so read both counters. Overlay glyph shaping is not counted as base-row preparation. Counters are process-wide and disabled without the existing opt-in.

Manual acceptance, requiring separate authority to launch or replace a client: preserve the original three large windows (2160x3765, 1577x918, 1655x2303), with nine panes in the main window. Record report deltas, `pidstat -t -p CLIENT_PID 1 20`, and the same 15-second `perf record -e cpu-clock:u -F 99 --call-graph dwarf,8192` profile for baseline and candidate under matched activity. Capture GPU telemetry separately; CPU counters do not prove GPU savings.

Run settled idle, AI-border-only, cursor-only, one-row edits in one pane, one actively scrolling pane, hidden-tab output, visibility regain, and multiple active agents. After cache warmup, border/cursor-only frames must increase root/overlay counts without base-row preparation; one-row edits should prepare that row rather than unchanged siblings. Scrolling/full damage may legitimately prepare the viewport. Hidden output alone must not add grid frames; prompt/CI/status deadlines and visible badges remain legitimate chrome work. To test fully parked rendering, disable cursor blinking, live status stats and eligible chrome clocks as well as animation. Repeat static `pulse_ms=0` and reduced motion.

Exercise split-scroll, old/new cursor positions, selection/vi, IME composition, find and hover, font/theme/opacity reload, resize/DPI moves, Kitty/Sixel ordering, image deletion/budget pressure, and session close/regain. Use existing `terminal::`, `terminal_element::`, `ai_indicator::`, `scrollbar::`, and root-synced-child headless checks, plus the registered terminal-image renderer probe and visual suites. No live run or numeric CPU/GPU improvement is asserted by this implementation.

GPUI already wraps each shaped line in a native paint layer. Its [paint-layer contract](https://github.com/zed-industries/zed/blob/f96212f2c50f54d93712fa130d6226b1ce7d76b5/crates/gpui/src/window.rs#L3715-L3738) batches non-overlapping geometry at one draw order. A whole terminal is not such a batch: equal-order primitives are sorted by type and sprites by atlas texture, which could break the Kitty/Sixel/text/overlay phases. The row cache retains these phases rather than wrapping the entire terminal in one unordered layer.

The ignored `benchmark_cell_color_resolution` test in [[crates/scribe-client/src/color.rs]] measures the actual resolver with default, indexed and truecolor inputs, mixed SGR flags, one warmup and seven timed samples. Run with the client package optimized:

```bash
cargo test -p scribe-client --lib \
  --config 'profile.dev.package.scribe-client.opt-level=3' \
  benchmark_cell_color_resolution -- --ignored --nocapture
```

On the 64-logical-CPU workstation, three interleaved baseline/fixed runs produced the following medians of run medians. Each timed sample resolved 4096 cells over 64 passes; inputs and outputs use `black_box`. Both client test builds used the package optimization override above. Nanoseconds per cell are integer-truncated.

| Workload | Before ns/cell | After ns/cell | Speedup |
|---|---:|---:|---:|
| Default colors | 71 | 7 | 10.1x |
| Indexed colors | 85 | 10 | 8.5x |
| Truecolor | 171 | 12 | 14.2x |

All 904 client-library tests and 188 client-binary tests passed, including the alpha/opacity regression, and client all-target/all-feature Clippy passed with warnings denied. These are CPU color-resolution measurements, not full-frame or input-latency results. Live deployment still needs explicit approval; a microbenchmark cannot establish that the running application's reported lag is resolved.

#### Dev Runtime Validation

Dev-only validation compares the updated client with the old release on the same server and matched 1600×1000 windows, using owned disposable sessions. Stable Scribe remains untouched.

The installed dev client matches the tested release build, SHA-256 `5271caf953d778ddaf7fdda848a6ca7593e88082b1ff2e66c7f91b47e8b709a5`. Both old and new executables run under the `scribe-dev` identity. This is a release-to-current comparison, not an isolated color-only binary A/B.

After external ffmpeg memory pressure cleared, the controlled run recorded:

| Metric | Updated dev | Old release |
|---|---:|---:|
| Render cadence | 60.025 fps | 59.628 fps |
| Inferred dropped frames | 0 / 483 (0%) | 2 / 483 (0.414%) |
| UI-thread CPU | 14.25% | 16.25% |
| Total client CPU | 98.25% | 102.62% |
| Median key-to-PTY echo, 60 samples | 0.272 ms | 0.290 ms |

Scrolling used a FIFO-gated `seq 1 1000000000` writer, a two-second settle and an eight-second measurement. The gate proves the owned shell reached the workload without relying on desktop keyboard focus. Byte counters advanced across both measurement windows. CPU percentages use one core as 100%; most sustained-output CPU was on the IPC/parser thread, not the UI thread.

The keyboard-echo run used the existing perf rig with window-targeted input, verified dev-window ownership and 60 captured samples per arm. The first keyboard-started scroll attempt was invalid because its shell never ran the command; it was not counted as a pass. The rig also failed to close a detached seed without attaching first, so owned sessions were explicitly attached and closed afterward. Test-created restore snapshots were archived outside the active dev store.

Host load was 11-12 on 64 logical CPUs with roughly 94 GiB available and zero averaged memory/I/O pressure at both measurement boundaries. Both clients met the scroll budget under these conditions; earlier stressed-host failures therefore do not establish a client regression. The probe measures application render cadence and key-to-PTY echo, not compositor presentation latency. This single-pane check does not reproduce the original nine-pane AI workload, and unmeasured startup/memory metrics leave the full launch gate incomplete.

### GPUI Box-Drawing Rasterizer

 ports the procedural rasterizer that emits a cell-sized RGBA alpha mask for U+2500–U+259F;  selects those codepoints.

Per the  capability spike, `TerminalElement` paints this mask as a foreground-coloured quad overlay after cell backgrounds and before shaped text, keeping edge-to-edge coverage regardless of font availability.

GPUI cannot upload a per-cell RGBA texture the way the wgpu atlas did, so  reduces the mask to the smallest set of uniform-alpha  rectangles that reproduces it exactly — horizontal runs first, then merged vertically. That reduction is what makes the overlay affordable: a full block becomes one quad instead of one per scanline, so a screen of box drawing costs a handful of quads per cell rather than hundreds.

### GPUI Window Opacity

`appearance.opacity` is a pure repaint in the GPUI client: the native surface is always alpha-capable and the configured value only scales the alpha of the backgrounds Scribe paints into it. See  for the reload seam.

 owns the derivation.  saturates out-of-range values into `0.0..=1.0` and maps NaN to fully opaque, so a malformed config degrades to a normal window instead of an invisible one — the config file itself is never validated on load, so every consumer clamps.  and  fold that value into a background's alpha, while  passes foreground colours through untouched. That split mirrors the legacy renderer's , which scaled each cell's background alpha and never its glyphs, so text stays readable over whatever the desktop shows through.

Two rules make the result equal the configured number rather than an accumulation of it. First, the window is opened with `WindowBackgroundAppearance::Transparent` unconditionally, even at opacity 1.0: surface capability is fixed at creation, so deriving it from the startup value would force a restart to ever go translucent — the legacy client's `window_transparent` flag had exactly that wart and refused live changes (). At 1.0 every painted background is alpha 1.0 and the window is pixel-identical to an opaque one. Second, the root element paints nothing at all. The titlebar, terminal grid and status bands tile the window edge to edge, so each pixel carries the opacity alpha exactly once; filling the root as well would composite a translucent band over a translucent root and land at 0.98 for a configured 0.85.

The alpha-aware surfaces are the terminal grid, titlebar and tab bar, prompt bar, and window status bar. Their colours come from the resolved theme rather than the literals the spike hardcoded, so a `theme` edit repaints the grid and chrome together.

## GPUI Cell-Accurate Paint Path

The GPUI terminal grid prepares changed rows and retains their resolved colors and shaping; overlays reuse those rows while maintaining independent cursor, selection and IME paint.

`Content` carries a `Cell` per grid position (character, raw `vte::ansi::Color` foreground and background, and the alacritty `Flags` bitset) rather than a `String` per row. The colours stay unresolved in the snapshot on purpose: a theme edit then repaints existing scrollback without re-running the parser, because a theme change invalidates prepared rows without rerunning the parser.

The retained base canvas keeps image/background/text phases together rather than making independently layered row elements. The uncached overlay canvas follows it. The image probe can still use `TerminalElement::paint` to exercise both passes together.

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

The primary family is self-contained as well:
[[crates/scribe-client/src/fonts.rs#register_embedded_fonts]] registers the
unmodified JetBrains Mono 2.304 regular, bold, italic and bold-italic faces before
GPUI can cache any family lookup, in both terminal and settings startup paths.
The default therefore works without any host font installation or runtime
network request. Debian stable/dev and macOS packages ship the upstream OFL
notice alongside the embedded fonts' own metadata.

A per-glyph fallback chain does not protect a missing primary-family lookup.
[[crates/scribe-client/src/terminal_element.rs#GridFont#resolve_family]] calls
[[crates/scribe-client/src/fonts.rs#terminal_font_family]] once at window creation
and each font/zoom reload, retaining an available user family (with canonical
case) or selecting the bundled primary with a warning. It never rewrites the
saved config or enumerates fonts on the row/frame hot path. The 0.1.14 failure
and clean-font reproduction are recorded in
[the fresh-install incident](../docs/solutions/runtime-errors/fresh-install-missing-font-and-wayland-controls.md).

The list mirrors the legacy cosmic-text atlas (`SCRIBE_COMMON_FALLBACKS` in `crates/scribe-renderer/src/atlas.rs`): `Symbols Nerd Font Mono`, `Symbols Nerd Font`, `Nerd Font Symbols Mono`, `Nerd Font Symbols`, then the generic sans / mono / symbol / emoji families. `Unifont Sample` is deliberately excluded — its private-use mappings turn an unavailable icon into an unrelated sample glyph, which is worse than a visible tofu box. `specs/016-gpui-client-rebuild/spikes/nerd-font-fallback-ordering.md` records the capability spike.

#### Embedded Symbols Font Defeats GPUI Face Eviction

Carrying the chain is necessary but not sufficient on this GPUI revision: a stock symbols-only family can never enter it, so the client embeds a patched `Symbols Nerd Font Mono` that can.

`CosmicTextSystem::load_family` (gpui rev `f96212f`, `crates/gpui_wgpu/src/cosmic_text_system.rs`) drops any face whose charmap has no `'m'` glyph and calls `db_mut().remove_face` on it. Every stock `Symbols Nerd Font*` face fails that test, so each chain entry resolves to nothing and the face is evicted from the font database outright. Omitting the families does not help either: GPUI builds its `FontSystem` with cosmic-text's default `PlatformFallback` and exposes no equivalent of the legacy  `forbidden_fallback`, so automatic fallback picks `Unifont Sample` — the exact font the legacy renderer banned.

 therefore registers  — the upstream binary with a `U+006D` cmap alias added by `tools/patch-nerd-symbols-font.py` — with GPUI's text system before the first frame is shaped. The face passes the `'m'` check, keeps its upstream family name so the chain resolves it, and covers the icon ranges even on hosts with no Nerd Fonts installed. The alias never leaks into visible text: the chain is only consulted for codepoints the primary font lacks, and every terminal font covers `m`.

Live capture on the real client confirms the chain is live: `U+F09B` and `U+F121` — absent from the primary JetBrains Mono — render as the octocat and code icons from the embedded face instead of `Unifont Sample` hex boxes, alongside the `U+E0B0`/`U+E0B2`/`U+E0A0` powerline glyphs.

#### Color Emoji Face Admission

The vendored WGPU text system admits recognized color-emoji faces without requiring a Latin `m` glyph, so loading the explicit fallback chain does not delete Noto Color Emoji from the font database.

Recognition is by PostScript name: `NotoColorEmoji`, `TwemojiMozilla`, `AppleColorEmoji` and `SegoeUIEmoji`; upstream knew only the first.

[[third_party/gpui-retained/gpui_wgpu/src/cosmic_text_system.rs#CosmicTextSystemState#load_family]]
uses its existing emoji-face recognition when applying the upstream no-`m`
filter. Other face filtering and fallback ordering stay unchanged. This is a
font-loading correction independent of retained rendering: both direct and
retained paths can use the installed color font. No font is installed or fetched
at runtime. Visual comparisons must pair equivalent font-loading behavior,
rather than treating newly available emoji as a retained-layer regression.

### Ligature Toggle

`appearance.ligatures` selects the OpenType features the runs are shaped with: `FontFeatures::disable_ligatures()` (`calt` off) when false, the font's own defaults when true.

The setting is read by  on the live config-load path, and `font_params_changed` already counts it as a font metric, so saving the edit repaints the grid without a restart — the same reload seam as `font` and `font_size`. `specs/016-gpui-client-rebuild/ligatures-spike.md` records the capability spike.

## Chrome Rendering

GPUI owns chrome layout, clipping, scene batching, and presentation;
`scribe-client` supplies theme-derived element styles and terminal-specific
canvas primitives.

The terminal's outer frame belongs to the OS, not an imitation titlebar.
[[crates/scribe-client/src/native_window.rs#relaunch_command]] checks Linux
Wayland decoration capability before GPUI or session IPC starts. Compositors
advertising `zxdg_decoration_manager_v1` keep native Wayland. Without it (GNOME),
a verified EWMH-managed X11 display supplies the desktop's actual native frame
through a one-time re-exec with `WAYLAND_DISPLAY` removed only from that process.
The two-second-bounded probe refuses startup with an actionable error when
neither native-frame path exists, rather than opening another borderless window.
X11/headless and non-Linux paths are unchanged; settings retains its independently
owned client decorations. No launcher override, environment mutation, custom
terminal controls or server restart is required. See
[the fresh-install incident](../docs/solutions/runtime-errors/fresh-install-missing-font-and-wayland-controls.md).

[[crates/scribe-client/src/main.rs#TerminalView#render]] builds the window shell
from GPUI elements for title bars, panes, dividers, prompts, status bands, and
dialogs. [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_preedit]]
adds the IME backdrop, shaped text, and underline through the same GPUI canvas;
it does not allocate a separate pipeline or shader.
