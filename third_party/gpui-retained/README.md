# Retained terminal bases

A Linux/WGPU rendering path, on by default through
`appearance.retained_gpu_bases`, measured on a single NVIDIA/Vulkan host.

## Provenance

Copied `crates/gpui` and `crates/gpui_wgpu` from
https://github.com/zed-industries/zed at revision
`f96212f2c50f54d93712fa130d6226b1ce7d76b5`.
Each crate retains upstream `LICENSE-APACHE`. Root `NOTICE` records attribution.

Everything the build needs is here; upstream's `examples/`, `docs/`, `tests/`
and `benches/` are not, because nothing in Scribe compiles them and they were
most of the copied bytes, a single example GIF being 4.3 MB of them. The
`[[example]]` and `[[bench]]` manifest targets that named those files were
removed with them, so no target dangles. `gpui/resources/` stays because
`gpui/build.rs` reads it.

Upstream's own `gpui` unit tests cannot run here in any case: they
`include_bytes!` fonts from Zed's repository-root `assets/` directory, which is
not part of either crate. Lint these packages with `--lib`, as the project's
gates do, rather than `--all-targets`.

This directory has its own workspace. Its manifest copies inherited dependency,
edition, publication and lint settings from that upstream revision. Upstream
workspace-relative dependencies become git dependencies at the same revision,
except the local `gpui` member. Scribe's root Cargo patch selects these two
packages for every upstream dependent. Other Zed packages remain unchanged.
The Cargo checkout is not modified.

## Local changes

- `gpui/src/window.rs`: paint-only `paint_retained_layer` capture API.
- `gpui/src/scene.rs`: immutable Linux layer reference, replay and batch ordering.
- `gpui_wgpu/src/wgpu_renderer.rs`: capture preparation, composite pipeline and
  cache lifecycle wiring.
- `gpui_wgpu/src/wgpu_renderer/retained_gpu.rs`: per-renderer texture cache,
  ordered capture submissions, direct fallback and opt-in counters.
- `gpui_wgpu/src/gpu_timing.rs`, `gpui_wgpu.rs`, `wgpu_context.rs`: dev-only
  timestamp feature negotiation and bounded asynchronous GPU readback.
- `gpui_wgpu/src/shaders.wgsl`: capture origin transform and premultiplied
  composite, without repeating text gamma or opacity correction.
- `gpui_wgpu/src/cosmic_text_system.rs`: separate font-admission correction;
  recognized emoji faces need not contain a Latin `m`. This affects direct
  rendering too and does not depend on the retention flag.

Scribe calls the API only on Linux, and only while
`appearance.retained_gpu_bases` is set. The client publishes that value at
startup and on every config reload, so an installed binary honours it and can
turn it off without a rebuild; `SCRIBE_DEV_RETAINED_GPU=0|1` overrides the
saved value for the A/B rigs. Other platforms keep the existing terminal base
path and do not compile this one.

## Capture contract

A view owns one base. Each live capture gets a new revision. GPUI cached-view
replay holds one shared inner scene reference instead of replaying its glyphs.
The renderer composites a valid texture without uploading the inner scene.
A first unchanged cached mount can still refill after an uncached changed mount.
Texture admission waits until an owner repeats its revision on consecutive
appearances. New or changing bases paint directly immediately; display cadence
and terminal publication are unchanged. Revision history is pruned with absent
owners and cleared with renderer invalidation. Existing allocations may remain
resident while their owner changes, but stale revisions never composite.

That two-frame rule is not enough on its own: a pane producing output can pause
for exactly one frame, which earns a texture that its next frame invalidates,
forever. Concurrent output recorded captures against zero hits for this reason.
So an owner whose texture is replaced having never been composited must show
twice the stability before the next capture, doubling to a bounded ceiling,
while any reuse resets the requirement. A pane that genuinely settles is still
retained within a few frames, and a busy pane stops paying for textures nothing
reads. The reuse flag is cleared when an allocation is repurposed for new
content, so an owner cannot keep an old flag alive by reusing the same texture.

The capture closure must contain paint only and enclose all clipped output in
its bounds. Existing `paint_layer` scopes use direct painting. Shadows, paths,
subpixel glyphs, native surfaces and nested captures fall back before publication.
Scribe also excludes terminal-image definitions or placements. Quads, grayscale
glyphs, polychrome glyphs and underlines are supported by the prototype; the
visual checks below cover a bounded fixture, not general acceptance. Cursor,
selection, IME, scrollbar, annotations, border
animation and outer background remain outside the capture.

Only RGBA8/BGRA8 WGPU targets retain textures. Other target formats draw the
supported inner scene directly. Captures use the existing pipeline and matching
globals: premultiplied mode multiplies RGB in the shader; other modes multiply
RGB through the pipeline's SrcAlpha blend factor. Both store premultiplied
attachment pixels and use a factor-One composite. The window surface's alpha
mode does not determine the offscreen texture representation. Capture bounds
round outward to device pixels. Shaders restore window coordinates for fragment
calculations after shifting vertex positions into the capture texture.

Each renderer has a 64 MiB logical texture budget. It prunes absent owners,
reuses equal-sized allocations and falls back for oversized or overbudget layers.
Resize/intermediate invalidation and renderer teardown clear the cache.
Driver-held references to in-flight frames can temporarily exceed the logical
budget. Capture submissions precede main-frame buffer writes. A revision becomes
valid only after submitting its completed capture. Existing renderer recovery
must force live painting when atlas handles are reset; this remains a required
runtime verification case.

## Diagnostic counters

`GPUI_RETAINED_LAYER_PROBE=/absolute/path` enables a process-wide counter snapshot
written at most every 200 ms after successful frame presentation calls:

- `captures`: submitted captures.
- `hits`: valid existing textures found before capture preparation.
- `composites`: successful retained-layer composite encodings, including misses
  captured earlier in the same frame.
- `fallbacks`: direct inner-scene encoding attempts when no valid GPU entry exists.
  Application-side image exclusions and rejected captures are not counted here.
- `upload_span_bytes`: aligned inner instance spans, including overflow retries.
  This is not an exact byte count of all GPU transfers.
- `live_bytes`: logical owned texture bytes across all renderer caches.
- `capture_operations`: paint operations in submitted captures.
- `capture_deferrals`: first-seen or changing revisions painted directly instead
  of captured. These direct encodings also contribute to `fallbacks`.

Counters do not establish lower GPU utilization or completed presentation FPS.

## GPU timestamp diagnostics

Linux `scribe-dev` with `SCRIBE_DEV_GPU_TIMESTAMPS=1` requests timestamp queries
only when the adapter supports them. This flag is independent of retention,
so the same binary can measure disabled and enabled terminal bases.

Four main-frame and 32 capture readback slots per renderer bound allocation.
Main timestamps span the first main pass through the last continuation,
including intermediate path rasterization. Each retained capture is timed
separately. Aborted overflow encoders release their reserved slot without
mapping it. Submitted results map asynchronously and are collected with
nonblocking device polling on later frames. Busy pools skip measurement rather
than stall rendering. Mapping/poll failures and reversed timestamps are counted.

The existing probe adds cumulative `gpu_main_samples`, `gpu_main_ns`,
`gpu_capture_samples`, `gpu_capture_ns`, `gpu_timestamp_skips` and
`gpu_timestamp_errors`, plus `gpu_main_samples_r<n>`/`gpu_main_ns_r<n>` per
renderer. Process-global totals mix windows of different sizes and frame
shares, so only the per-renderer split can show whether a given window's
frames changed cost. Each renderer logs its index with its surface size,
because index assignment follows window creation order and is not stable
across runs. The split adds no GPU work. Measurements lag submission; pending samples may be
lost on renderer teardown. Accept timing comparisons only with demonstrated
coverage and no skipped or failed samples in the measured interval. Compare
instrumentation enabled and disabled to assess measurement overhead.

These are GPU elapsed times for renderer passes, not utilization percentages.
Queue buffer uploads, presentation/compositor work and other processes are
outside the measured intervals. Driver scheduling can affect elapsed times.
The private idle NVML probe returned framebuffer memory and dash utilization
placeholders. Active native workloads also returned numeric utilization, but
coverage was intermittent. Neither missing values nor GPU elapsed times can
be treated as a complete utilization comparison.

## Measured scope

The final font-corrected candidate's three-window, eleven-session native
off/on/off comparison reduced border-animation CPU from 87.99/82.74% of one
core to 50.12%, and one-pane-output CPU from 27.25/26.87% to 16.50%.
Concurrent-output CPU rose from 77.87/77.12% to 84.87%; that phase recorded
70 captures, 2,683 deferrals and zero hits. Stable border rendering had zero
captures/uploads and 5,311 texture hits. Border damage cadence rose from about
41-44 to 59.3 notifications/s/window, and border GPU pass time rose from
66.38/78.46 to 91.27 ms/s.

Those per-second totals compare unequal amounts of delivered work. Retention
raised border throughput to about 177-182 frames/s against 126-137 for the
controls, because the control is CPU-bound and drops frames the retained build
delivers. Three repeated off/on/off runs with per-renderer attribution measured
cost per frame for the same 2160x3765 window instead. Every retained sample was
below every control sample in all three workloads: border 1034-1173 us against
1259-1495, one-pane output 2123-2195 against 2240-3376, and concurrent output
1570-1637 against 1764-2041, with medians 17.7%, 29.7% and 13.9% lower. CPU per
frame fell 58.1% and 46.4% for border and one-pane output.

So the earlier per-second GPU increase reflects delivered frames, not a more
expensive frame. The one measured per-frame regression is concurrent-output CPU
at 3.9%, where captures are attempted and never reused. Absolute per-frame times
drift substantially between runs and controls spread by over 20% within a run,
so only within-run sequential comparisons on the same binary are used, and
single-run direction claims are not. A separate temporary in-pass probe measured
retained-layer batches at 9.2-9.3% of main pass time and all draw commands at
98.8-98.9%, so render-pass clear and store boundaries are not the cost; that
probe has been removed. Logical textures used 44.45 MiB; framebuffer high-water
marks do not isolate retained allocation cost. These short samples still do not
establish production acceptance, energy use, or behaviour on other GPUs,
drivers, or platforms.
Evidence and fixture qualifications are tracked in `scribe-em0s` and `scribe-zvhh`.

Private Xvfb baseline/off/on comparisons covered shrinking, growing, restored
size, same-client minimize/regain and selection. All six phases showed retained
hits without settled recaptures. Default and image-fallback crops matched
exactly; retained text differed by at most one channel level against a
predeclared two-level limit. The same six phases passed at 1.25x X11 scale,
with 19-30 existing hits and zero settled recaptures per phase. That comparison
keeps the cursor hidden and drives repainting through processing-border hooks;
cursor blinking cannot be reliably excluded with fixed crop margins across DPI.
A separate private 6600x3000 fixture verifies single-layer budget refusal
(zero logical bytes, direct pixels exact) and aggregate refusal (one admitted
layer at 37,807,216 bytes while its sibling falls back). Shrinking to 1201x901
admits both layers at 3,602,940 bytes. Adding an image excludes one owner and
prunes its texture, reducing usage to 1,803,060 bytes; removing the image
restores both layers. Settled captures remain zero, hits resume, image crops
are exact and retained text stays within one channel level. This covers
logical admission and absent-owner pruning, not driver physical-memory limits
or an LRU eviction policy.

Private logical-device recovery also passes. A temporary, explicitly gated
hook destroys only the disposable client's WGPU device and sets its loss flag
(intentional `Destroyed` callbacks otherwise do not request recovery). The
existing platform recovery path recreates the context and atlas. One new
deferral/capture precedes resumed hits, logical bytes return to 3,711,060, and
before/after styled-text and colored-glyph crops match exactly. Client/session
identity survives. The hook and staged diagnostic binary were removed with
byte comparisons against their backups. This does not test a physical GPU
reset, driver crash, suspend/resume or multi-window recovery.

Native X11 owned-window RGBA comparison passes parity: disabled controls are
exact, retained text differs by at most one level, and settled hits occur with
no recaptures. The separate opacity criterion fails in all three cases:
background alpha is 255 despite configured opacity 0.95. This NVIDIA/Vulkan
surface reports only `Opaque` presentation support, which ignores texture
alpha during compositing. `scribe-5f2i` tracks this baseline/platform blocker;
no unsupported alpha mode is forced. Captures use only PID/title-verified named
window pixmaps, never the desktop/root drawable. A synthetic backdrop preview
is not evidence of final desktop compositor output.

Private 1.25x overlay checks cover hidden, steady block, underline and bar
cursors, a cursor positioned on a wide colored glyph, and real IBus CangJie
preedit/commit. Both disabled controls are exact; retained crops differ by at
most one level. Cursor/preedit changes leave capture count unchanged; committing
`我` updates PTY content and causes one new capture. Raw composition keys never
reach the PTY. This tests parity with direct rendering, not blink-phase timing
or independent correctness of the baseline wide-character cursor geometry.

Alpha-preserving native presentation remains unqualified. Independent source
reviews found no P0/P1 defects in the retained-layer seams; the alternate-font
repeatability and physical/multi-window recovery limits above remain.

Selecting installed Noto Color Emoji initially did not establish polychrome
coverage: the pinned font loader removed faces without an `m` glyph. The
`scribe-il67` correction exempts already recognized emoji faces. With JetBrains
Mono primary and the corrected loader in both controls, the six 1.25x lifecycle
phases visibly render colored emoji and pass exact disabled/image crops plus
one-level retained-text differences, with actual hits and no settled recaptures.
The noninteractive fixture disables PTY echo so image protocol replies cannot
alter its screen content. The alternate-font fixture still failed exact
direct-pane comparison. A same-binary disabled/disabled diagnostic found differing atlas
placements and pixel output despite matching destination geometry and uniforms.
`scribe-sp72` tracks this cross-launch nondeterminism. Its cause and exact-fallback
acceptance remain unresolved; no comparison threshold has been relaxed.

## Acceptance status

Completed private evidence includes Linux workspace/vendor build gates and
regressions, matched default/image-fallback crops, styled monochrome and colored
text, fractional-DPI resize/regain/selection, steady-cursor/real-IME overlays,
logical budget refusal/owner pruning/readmission, logical-device recovery, and
owned native RGBA parity. Matched six-workload native measurements preserve
animation policy but show conditional CPU gains, not the required CPU AND GPU
cost reduction.

Remaining acceptance gaps include alpha-preserving native presentation
(`scribe-5f2i`), alternate-font exact-fallback repeatability (`scribe-sp72`),
physical/suspend/multi-window recovery, blink-phase timing, and broader
unsupported-content/cross-platform verification. Logical owner pruning is not
an LRU eviction policy or proof of driver memory reclamation. Keep the
experiment opt-in; do not promote partial CPU gains as end-to-end acceptance.
