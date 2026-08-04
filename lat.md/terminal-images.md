# Terminal Images

Terminal images are server-owned, bounded Kitty and Sixel state derived from untrusted PTY bytes and rendered from immutable typed client state.

The frozen source contract is `specs/020-terminal-images/contract.md`; its
machine-readable values and owned fixtures live under
`tests/e2e/fixtures/terminal-images/`.

## V1 Protocol Contract

V1 accepts direct-inline Kitty RGB, RGBA, PNG and xterm-compatible Sixel without exposing host resource loaders.

Kitty uses 7-bit APC `G`/ST, direct `t=d`, formats `f=24/32/100`, RFC 1950
`o=z`, official 4096-byte `m=1/0` chunks, and actions `t/T/p/q/d`. Classic
crop, cell span, pixel offset, cursor movement, signed z-order, and Unicode
`U=1` placeholders are supported. Placeholder identity uses `U+10EEEE`,
official diacritics, foreground image id, underline placement id, and
specified left-cell inheritance. The IPC placeholder background byte is
reserved compatibility data, not a second image-opacity channel.

Kitty delete selectors are exactly `a/A`, `i/I`, `p/P`, `x/X`, `y/Y`, and
`z/Z`. Any delete aborts an incomplete transfer. `q=0` permits success and
failure replies, `q=1` suppresses success, and `q=2` suppresses all replies.
Retransmitting an image id replaces its data and placements; reusing an
image/placement pair replaces that placement.

Physical Kitty and Sixel row, column, and cell deletes select against the
placement's effective clipped cell extent, not only its anchor. Virtual
`U=1` placements ignore all, placement, cell, row, column, and z selectors;
only an image selector carrying their explicit image id removes them. A hard
delete frees only definitions selected by placements it actually removed,
once no placement references them. An explicit image-id delete can free an
unplaced target. Unrelated unplaced definitions and data used by a protected
virtual placement survive every other hard scope.

Kitty images below z `-1073741824` paint beneath non-default backgrounds;
other negative images paint above backgrounds and below glyphs; nonnegative
images paint above glyphs. ED2 clears visible placements, RIS clears all image
state, 1049 owns an empty alternate-screen image set, and other text erases do
not affect Kitty graphics.

Sixel accepts 7-bit and C1 DCS/ST, `P1;P2;P3 q`, sixel bits, repeat, raster
attributes, 256 palette selection and HLS/RGB definitions, `$`, and `-`.
`P2=0/2` paints zero bits with the background; `P2=1` preserves them. Runtime
DA1 adds attribute 4 exactly once.

V1 excludes Kitty indirect transports, C1 APC, file/temp/shared-memory/URL or
network loading, animations, frame composition, relative-parent placement,
image-number allocation, iTerm2, ReGIS, vector/video, protocol translation,
Windows, and guaranteed tmux/GNU screen/Zellij passthrough.

## Trust Boundary and Limits

ImageLimits rejects untrusted work before allocation, retention, IPC replay, or GPU upload with checked arithmetic and per-session isolation.

| Field | Exact v1 ceiling |
| --- | ---: |
| control string | 16,777,216 bytes |
| Kitty chunk payload | 4,096 encoded bytes |
| chunks per transfer | 32,768 |
| accumulated encoded transfer | 89,478,488 bytes |
| base64-decoded transfer | 67,108,864 bytes |
| inflated output | 67,108,864 bytes |
| width / height | 4,096 pixels each |
| pixels | 16,777,216 |
| canonical RGBA definition | 67,108,864 bytes |
| definitions per session | 128 |
| placements per session | 1,024 |
| retained session CPU data | 134,217,728 bytes |
| projected GPU data per view | 268,435,456 bytes |
| retained image data per process | 536,870,912 bytes |
| concurrent decodes per process | 2 |
| queued decodes per process | 8 |
| queued encoded data per process | 134,217,728 bytes |
| work per command | 134,217,728 units |
| queue wait | 1,000 monotonic ms |
| decode | 2,000 monotonic ms |
| replay/handoff chunk | 1,048,576 bytes |
| deadline check interval | 4,096 work units |

One work unit is one input byte examined, output byte emitted, or pixel write,
charged cumulatively. Deadline checks occur during work, not only afterward.
The 64 MiB image ceiling is one 4096-square RGBA definition; larger session,
view, and process caps account separately for copies and concurrency.

Framing charges bytes before append; transfer/base64/inflate checks guard each
buffer growth; dimension and pixel checks precede allocation; session/process
retention checks precede commit; view projection precedes IPC/upload. Queue
depth and bytes are charged before enqueue, then queue and decode deadlines use
monotonic time.

## Capability Lifecycle

Capability is latched server session state, so viewer churn cannot alter protocol replies or authoritative image behavior.

A session starts text-only and unlatches. A capable creator or first explicit
capable enable latches exact v1. A latched viewerless session keeps parsing,
replying, and retaining bounded state. Every Kitty result and DA1 response is
written once by the server to the originating PTY in input order.

An incapable viewer gets typed `capability_mismatch` instead of a degraded
attach. Effective advertising intersects compile support, decoder health,
viewer capability, and the master switch. Disabling cancels queued/active work,
clears partial and committed state, removes DA attribute 4, emits no Kitty
discovery reply, then unlatches. Re-enable needs a new capable latch.

Capable handoff preserves latch, generation, committed state, and bounded
partial framing. Unsupported downgrade fails typed; detach, replay, reconnect,
handoff, and multiple viewers never synthesize duplicate PTY replies.

## Typed IPC Contract

Shared image types keep local compatibility while giving remote peers one exact bounded wire contract.

[[crates/scribe-common/src/terminal_images.rs#ImageLimits]] freezes every v1
security ceiling in code. Canonical definitions carry typed IDs, generation,
checked RGBA dimensions, and byte length. Actual bytes travel only through a
custom-deserialized `BoundedImageBytes` chunk capped at 1,048,576 bytes; the
contract cannot represent paths, URLs, shared-memory names, or other indirect
loaders.

Placements carry protocol, typed image and placement identity, generation,
cell anchor, source crop, destination cell extent, pixel offsets, z-index,
scroll/cursor behavior, an optional exclusive logical-cell clip, and bounded
placeholder metadata. The clip defaults absent for replay compatibility and
preserves original source mapping through repeated scroll and resize. Typed
grid effects bind image-driven cursor, scroll, erase, resize, screen, and reset
consequences to the same ordered client operation stream.

Common placement validation rejects empty geometry, inconsistent
protocol/kind/placeholder combinations, and invalid clips before replay or
client ingestion. A clip is nonempty, bounded by exclusive cell coordinate
65,536, forbidden on virtual placeholders, and contained by the placement's
logical envelope. That envelope adds one right or bottom cell when the
corresponding pixel offset is nonzero.

Live IPC uses generation-tagged begin, update, and commit records under one
monotonic output sequence. Replay uses begin metadata, definition metadata,
bounded definition chunks, placements, and commit; every record repeats the
generation so a client can reject mixed snapshots before exposing partial
state. Replay begin validates definition, placement, and retained-byte totals.

Local `Hello` and `Welcome` image capabilities default every missing field to
false, preserving old/new MessagePack decoding. An incapable attach has a
typed capability mismatch with required and offered sets. Remote protocol v5
uses exact matching and a typed mismatch naming client/server versions plus
which endpoint must update.

## Sixel Chronology

Sixel mode behavior deliberately follows current xterm polarity instead of DEC's contradictory DECSDM description.

At DCS start the server captures cursor, margins, screen, `?80`, and `?8452`;
later PTY bytes wait for atomic commit/rejection. Under default `?80l`, the
raster anchors at the cursor and scrolls vertically at the bottom margin. At
ST, `?8452l` advances to the next complete row at the original column, while
`?8452h` advances immediately right of the raster.

Under `?80h`, the raster anchors at the page origin, crops without scrolling,
and leaves the cursor unchanged; `?8452` has no cursor effect. Rasters paint
above default background and below non-default backgrounds and glyphs. Grid
erase, scroll, trim, resize clipping, alternate-screen destruction, and RIS
apply in byte chronology. Resize clips without resampling; DECSTR preserves
committed raster and resets modes.

## Bounded Framing and Parsing

The graphics framer recognizes the frozen image subset across arbitrary PTY reads while preserving exact byte order for the existing terminal path.

`GraphicsFramer` runs inside the server image-state seam and holds at most one
bounded APC/DCS. A speculative DCS candidate retains raw header
bytes up to the control-string ceiling because a non-`q` final byte must pass
through exactly; its parallel Sixel parameter scanner uses three fixed slots,
one checked numeric accumulator, and field/status metadata. Seven-bit Kitty APC
and seven-bit/C1 Sixel DCS accept split introducers and terminators. CAN or SUB
cancels a recognized image string; EOF reports a typed truncated sequence;
over-budget input is discarded without retaining more payload until ST restores
the ordinary byte stream. A numeric Sixel-looking DCS header that crosses the
ceiling is charged through its offending digit or separator, converted to the
same failed discard state, and never returned as raw terminal bytes.

Recovery is overlap-safe: a repeated ESC can begin the real seven-bit ST, an
ESC followed by C1 ST ends with typed malformed framing, and an ESC/C1 control
after an abandoned introducer candidate is reprocessed from ground. Sixel's
three introducer parameters use constant scanner metadata and mark a fourth
field at its separator or numeric overflow at its offending digit. Once `q`
confirms Sixel, a malformed header seeds the active typed failure, so subsequent
body bytes are counted for termination recovery but never copied into the body
buffer. ST, CAN, SUB, and EOF extend the failure's exact source range without
replacing an earlier malformed or quota category; cancellation is malformed
framing only when no earlier failure exists.

Each result carries a half-open absolute raw-byte range. `Raw` events and
recognized `?80`/`?8452` mode annotations expose their exact terminal bytes for
one downstream feed; Kitty/Sixel commands and failures expose no terminal
payload. This lets the later ordered server integration suppress image strings,
forward every unrelated byte once, and preserve an auditable commit boundary.

Kitty parsing accepts only direct `t=d`, `f=24/32/100`, `o=z`, actions
`t/T/p/q/d`, v1 placement/delete controls, quiet/chunk flags, and encoded chunks
up to 4,096 bytes. Sixel parsing accepts the three DCS parameters, sixel data,
repeat, raster attributes, palette selection/HLS/RGB, graphics CR/newline, and
the exact xterm private modes. Unsupported or malformed controls return the
frozen typed category with safe limit identity only, never payload bytes.

## Server-Owned Session State Seam

One production `SessionTerminal` owns terminal-image ordering and state so later hardening extends one path instead of creating probe-only engines.

Each production PTY reader owns exactly one seam through
`PtyTerminalImageState`. The seam owns `GraphicsFramer`, generation and
sequence cursors, active screen, definition and placement maps, and
payload-free pending-transfer metadata. Sessions share one immutable v1
process policy.

Placement identity is keyed by screen plus the complete protocol identity
`(image_id, placement_id)`, matching client canonical state. Sequence
admission uses a checked upper bound of one non-raw boundary per input byte.
Normal reads parse directly, avoiding copies of retained transfer payloads;
only reads near sequence exhaustion use speculative rollback framing.

Each production PTY read enters `process_pty_reader_ingress`, which advances the
seam before invoking the existing client-delivery and `Term`-feed sinks exactly
once with the same effective bytes. The seam returns typed `Raw` or sequenced
`Image` boundaries in source order. Recognized Sixel modes also produce an
image-side boundary. No image fanout or PTY reply write-back is connected yet.

Payload-free work counters distinguish direct reads from speculative clones.
Transactional rejection preserves framer offsets, pending metadata, sequence,
screen, definitions, and placements; operational work counters may still
record the rejected rollback attempt.

## Typed Failures

Every rejection has a stable category and payload-free metadata suitable for diagnostics without leaking PTY image content.

Categories are exactly `policy_disabled`, `unsupported_protocol`,
`unsupported_action`, `unsupported_transport`, `malformed_framing`,
`malformed_control`, `malformed_payload`, `truncated_sequence`,
`chunk_mismatch`, `invalid_dimensions`, `quota_exceeded`,
`work_budget_exceeded`, `decode_deadline_exceeded`, `decode_cancelled`,
`decode_failed`, `image_not_found`, `capability_mismatch`,
`renderer_unavailable`, and `evicted`.

Kitty maps unsupported/malformed to `ENOSYS`/`EINVAL`, bounds to
`E2BIG`/`ENOSPC`, deadlines/cancellation to `ETIMEDOUT`/`ECANCELED`, decode to
`EIO`, and missing image to `ENOENT`, subject to quiet mode. Diagnostics carry
only protocol/action, safe dimensions/counts, and limit name; applications own
text fallback.

## Compatibility Corpus

Pinned applications and owned protocol fixtures prevent release claims from drifting with package repositories or external test suites.

Yazi is `v26.5.6` at
`aa526434f00bb44e2e902d9a4ac5f810da1018b9`: an unknown terminal's successful
Kitty query selects its `KgpOld` classic path without terminal spoofing. Chafa
is `1.18.2`, exercised with `--format kitty --probe off` and
`--format sixels --probe off`. gnuplot is `6.0.3`, exercised through
`set terminal sixelgd`. Each application runs through direct PTY and SSH.

Ten owned ASCII-hex fixtures cover Kitty query ordering, RGB, chunked zlib
RGBA, PNG, Unicode placeholders, deletion; 7-bit and C1 Sixel; xterm mode/text
chronology; and CAN/SUB malformed recovery. `fixtures.tsv` and `contract.json`
freeze every path and expected outcome.

## Contract Verification

Docker verification proves the frozen limits, matrix markers, app pins, and owned fixture inventory and emits reviewable contract evidence.

`tests/e2e/terminal-image-contract.sh` runs only under `SCRIBE_E2E_SANDBOX`,
checks exact security values and fixture ownership/hex integrity, then copies
the canonical JSON unchanged to
`test-output/terminal-images/contract.json`. Invoke it with
`just e2e-func terminal-image-contract.sh` after building the functional image.

## Framing Verification

Docker verification proves framing is invariant across PTY read boundaries and that recovery never consumes adjacent terminal text.

`tests/e2e/terminal-image-framing.sh` runs the production `scribe-pty` framer
through `scribe-test image-framing`. Every owned fixture is tried whole, at
every two-chunk byte split, and one byte per read. Every feed verifies the
fixture's complete parsed command expectation and contiguous raw-range tiling.
For every forwarded event, it also proves range length equals byte length and
the bytes equal the corresponding source-input slice; suppressed commands and
failures retain ranges without terminal bytes.
Adversarial cases cover both Sixel forms, split and overlapping ESC/ST and C1
controls, candidate resynchronization, CAN/SUB, malformed and unsupported
controls, fourth-field/overflow classification before a later body quota,
malformed-header recovery, mixed terminators, EOF truncation,
exact/over-budget strings, Sixel numeric-header max-plus-one discard, first-error
preservation across CAN/SUB, Kitty chunk limits, and bounded non-image DCS
passthrough. Boundary and cancellation cases cover every split, both Sixel
forms, exact failure ranges, adjacent text, and absence of raw payload leakage.
After every assertion passes, the probe atomically publishes schema-versioned,
payload-free audit evidence to `test-output/terminal-images/framing.json`, with
owned-fixture semantic/tiling counts and stable markers for each adversarial
case family.

## Bounded Decode Decision

Production decoding is a conditional go through narrow decoder-only forks and Scribe-owned limits; generic or stock whole-image entry points are outside the trust boundary.

The decode spike selects `flate2 1.1.9` low-level `Decompress` behind a
4,096-work-unit Scribe loop. The loop charges input and output, checks the
monotonic deadline and cancellation, rejects projected inflation, and grows
output only through fallible allocation.

Sixel vendors only `icy_sixel 0.5.0` decoder source and adds caller-owned
dimension, pixel, allocation, work, deadline, and cancellation limits. Its
encoder, `quantette`, and unaudited SIMD span paths are excluded. PNG vendors
only `png 0.18.1` decoder core and adds the same hook at compressed-input,
inflated-output, unfilter, and pixel-conversion boundaries; encoder, APNG, and
ancillary text/profile paths are excluded.

Generic `image 0.25.10`, stock `png`, stock `icy_sixel`, and C decoders are
no-go. The first two expose no cooperative hook at the frozen interval and
their allocation controls cannot alone enforce every Scribe trust budget.
`specs/020-terminal-images/decoder-decision.md` records source evidence, fork
ownership, evidence schema, and remaining production work.

## Bounded Sixel Decoder

The vendored decoder makes Sixel rasterization interruptible and fallible at every untrusted growth boundary without exposing partial images.

[[third_party/icy-sixel-decoder/src/lib.rs#decode_sixel]] vendors the decoder
concepts from `icy_sixel 0.5.0`, crates.io SHA-256
`85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5`,
at upstream revision `998cbb2c6d8ed5272f9cc4702a4660778972bf3f`.
`README.md`, `LICENSE-MIT`, and `LICENSE-APACHE` in that directory retain the
source URL, exact license texts, fork delta, excluded code, and Scribe
maintainer ownership for CVE and upstream-release review.

[[crates/scribe-image-decode/src/lib.rs#DecodeBudget|DecodeBudget]] carries the
shared frozen 4096-axis, 16,777,216-pixel, 67,108,864-byte
RGBA, 134,217,728-work-unit, absolute monotonic deadline, and 4096-unit check
interval boundaries. Caller hooks provide cooperative cancellation and an
allocation-accounting veto. Every dimension/add/multiply and canvas offset is
checked; each canvas allocation uses `try_reserve_exact` before mutation.

Input examinations, emitted RGBA bytes, and pixel writes charge cumulative
work. Cancellation and deadline checks happen at entry, at most every 4096
charged units, and before returning. Failure drops the private canvas and
returns a payload-free typed category, so no stale or partial result can
escape. Complete 7-bit/C1 DCS and already-framed payload entry points share
the same budget implementation.

The fork preserves raster attributes, repeat, `$`/`-`, 256 private palette
registers, RGB/HLS definitions, least-significant-bit sixel rows, and opaque
`P2=0/2` versus transparent `P2=1` backgrounds. Encoder and quantizer source,
`quantette`, unsafe/SIMD fills, full terminal parsers such as termwiz, and C
Sixel libraries are absent. `DEPENDENCY-TREE.txt` records the shared
Scribe-owned budget dependency and its `scribe-test` reverse edge.

## Bounded Sixel Decoder Verification

Docker verification exercises owned fixtures and adversarial allocation, arithmetic, work, cancellation, deadline, palette, raster, repeat, and malformed-input boundaries.

`tests/e2e/terminal-image-sixel-decoder.sh` invokes the production vendored
crate through the `scribe-test sixel-decoder` command. It covers 7-bit and C1
owned fixtures, background/palette/repeat/raster semantics, exact and
max-plus-one dimensions and growth, palette 255/256, deterministic allocation
denial, immediate and interval cancellation, expired deadline, truncated and
overflowed controls, plus exact and max-plus-one work accounting.

The run atomically writes
`test-output/terminal-images/sixel-decoder-evidence.json` with the contract
version, source revision/checksum/licenses, explicit exclusions, frozen limits,
typed outcomes, and an `all_passed` aggregate. Invoke it only through
`just e2e-func terminal-image-sixel-decoder.sh` after rebuilding the functional
Docker image.

## Shared Decode Budget

Kitty and Sixel charge one caller-owned cooperative budget, preventing either decoder from weakening work, cancellation, deadline, or allocation policy.

[[crates/scribe-image-decode/src/lib.rs#DecodeBudget]] owns cumulative work,
the next 4096-unit observation boundary, an absolute monotonic deadline,
caller cancellation and allocation hooks, and peak live-allocation evidence.
Checked work overflow, hook denial, cancellation, and deadline expiry return
typed payload-free errors. The Sixel and PNG forks use this exact type rather
than independently approximating the frozen interval.

## Bounded Kitty Normalization

Kitty direct payloads become canonical RGBA only after exact chunk, base64, transport, compression, dimension, and format validation.

[[crates/scribe-common/src/kitty_decode.rs#KittyTransfer]] rejects every
indirect transport before allocation, caps each 4096-byte chunk and cumulative
chunk/encoded/decoded counts, requires exact RFC 4648 padding boundaries, and
drops encoded bytes after each chunk. Optional RFC 1950 data uses low-level
`flate2 1.1.9` with 4096-byte caller storage, counter-derived input/output work,
projected-output checks, fallible growth, and no trailing compressed bytes.

Raw `f=24` and `f=32` require exact checked `s*v*channels` length before the
canonical allocation; RGB conversion writes opaque alpha. `f=100` accepts only
a PNG signature and delegates to the bounded decoder fork. Completed results
retain width, height, canonical RGBA, alpha metadata, and safe counts only;
base64, compressed data, paths, URLs, resource names, and partial results never
escape normalization.

## Bounded Kitty PNG Decoder

The decoder-only PNG fork retains static PNG semantics while exposing every untrusted allocation and work boundary to Scribe.

[[third_party/image-png-decoder/src/lib.rs#decode_png]] is pinned to `png
0.18.1`, crates.io SHA-256
`60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61`,
at upstream revision `2a3f980245e3ae38b82ade96533e7b450e8477bb`.
The adjacent README, exact MIT/Apache licenses, and dependency tree record
provenance, fork delta, exclusions, pure-Rust dependencies, and Scribe CVE and
upstream-update ownership.

The fork validates signature, chunk order/length/CRC, dimensions, color/depth,
IDAT completion, filters, transparency, and Adam7 with checked arithmetic. It
supports every legal static PNG color/depth combination and converts to RGBA.
Unknown ancillary chunks are CRC-checked then skipped without allocation;
unknown critical chunks, APNG, text/profile retention, encoders, generic image
selection, and indirect resource loaders are excluded.

## Bounded Kitty Decoder Verification

Docker verification covers supported normalization plus adversarial payload, resource, allocation, work, deadline, and decompression boundaries.

`tests/e2e/terminal-image-kitty-decode.sh` invokes the production normalizer
through `scribe-test kitty-decode`. Stable cases cover RGB, RGBA, exact chunk
accumulation, RFC 1950 zlib, PNG, malformed base64, padded non-final chunks,
raw-length mismatch, truncated/non-PNG input, every indirect source class with
zero allocations, deterministic allocation denial, cancellation, deadline,
and a valid stream expanding one byte beyond the frozen ceiling.

The run atomically writes schema-versioned
`test-output/terminal-images/kitty-decode-evidence.json` with exact dependency
versions, revisions, checksums, licenses, fork exclusions, frozen limits,
typed case outcomes, and `all_passed`. Run only through
`just e2e-func terminal-image-kitty-decode.sh` after rebuilding the functional
Docker image.

## Decode Spike Verification

Docker verification exercises exact decode limits and emits a reviewable decision plus structured evidence without implementing production decoders.

`tests/e2e/functional/terminal-image-decode-spike.sh` runs the harness-only
probe through `just e2e-func`. It loads frozen `contract.json` values and covers
exact max/max-plus-one dimensions, a real fallible maximum allocation,
deterministic allocation denial, cumulative work max-plus-one, cooperative
cancellation, deadline checks, zlib and PNG bombs, valid PNG decode, and
gradual Sixel canvas growth.

The run writes schema-versioned
`test-output/terminal-images/decode-spike-evidence.json` and
`decoder-decision.md`. Evidence contains only limits, typed outcomes, counts,
dimensions, allocation peaks, and compressed sizes; it never records image
payloads or decoded pixels.

## IPC Contract Verification

Docker verification freezes MessagePack bytes, bounded decode behavior, local handshake defaults, and both remote version directions.

`tests/e2e/fixtures/terminal-images/ipc.json` stores stable named MessagePack
hex for legacy/current local handshakes, live chunks, every replay phase,
capability refusal, and client-older/server-older remote mismatches.
`scribe-test terminal-image-ipc` decodes them through the production shared
types, checks maximum and maximum-plus-one bounds, round-trips a named clipped
placement, proves absent legacy clips remain omitted/defaulted, rejects
reversed, empty, out-of-range, placeholder, and protocol-mismatched placements, and writes
`test-output/terminal-images/ipc.json`. Invoke it with
`just e2e-func terminal-image-ipc.sh`.

## Client Live Scene Verification

Docker verification proves ordered atomic live state, cleanup, text filtering, and truthful mismatch plumbing before image painting exists.

`tests/e2e/fixtures/terminal-images/client-scene.json` drives production
[[crates/scribe-client/src/terminal_image_scene.rs#LiveImageScene]] records
through definitions, bounded chunks, placements, replacement, delete, scroll,
erase, hard reset, interrupted generation replacement, and stale rejection.
It also freezes placeholder-copy input and typed capability mismatch data.

`tests/e2e/terminal-image-client-scene.sh` invokes
`scribe-test terminal-image-client-scene`, writes
`test-output/terminal-images/client-scene.json`, and requires every evidence
field to pass. This is CPU-scene evidence only; it makes no renderer, replay,
server fanout, or settings claim.

## GPUI Lifecycle Verification

The isolated visual spike proves the selected shared-source crop and lifecycle path on Linux WGPU without implementing terminal image placement rendering.

`tests/e2e/visual/terminal-image-gpui-spike.sh` launches the guarded
`--gpui-image-spike` surface inside the visual Docker harness. The release
binary records GPUI's selected Linux WGPU adapter/backend, paints a
four-quadrant source twice through one `RenderImage`, and checks GPUI's source
identity across atlas invalidation. It uses
[[crates/scribe-client/src/gpui_image_lifecycle.rs#paint_cropped_image]],
requiring the cropped destination to remain green before and after atlas
invalidation and final-reference eviction.

[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache]] rejects
4097-by-1 metadata before constructing a `RenderImage`, uploads 1-by-1 and
4096-by-1 sources, shares one source identity across full and cropped
placements, and calls `Window::drop_image` for each final cache reference.
Evidence lands in `test-output/terminal-images/linux/gpui-spike.json` beside
the three compared captures and sanitized log. The chosen path and pinned
source audit prove that the shared identity is one atlas key, `drop_image`
deallocates it, and device recovery clears then lazily rebuilds that key. Those
facts are frozen in
`specs/020-terminal-images/gpui-lifecycle-decision.md`.

## Layered GPUI Renderer Verification

The Docker visual corpus verifies production placement paint, phase ordering, geometry, placeholder mapping, and final-reference cleanup on Linux WGPU.

`tests/e2e/visual/terminal-image-renderer.sh` launches the guarded
`--terminal-image-renderer-probe` surface at a real 2x GPUI X11 scale and
captures crop, scale, alpha, every paint phase, 8-bit inherited and 32-bit
placeholder mapping, Sixel text chronology, production scroll/margin effects,
cache pressure, eviction, pane close, and atlas recovery. It invokes the same
[[crates/scribe-client/src/terminal_element.rs#TerminalElement]] and shared
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache]] used by live
panes.

Evidence lands under `test-output/terminal-images/linux/renderer/`.
`renderer.json` records device-space coordinates plus observed and expected RGB
for every phase/geometry assertion, the platform scale factor, and pixel
comparisons. Pressure must reject the extra live source without an atlas drop
or pixel change at the hard cache bound. Distinct overlapping Sixels prove
later completion wins below text; a selected current match proves find
precedence above images.
Eviction and Linux atlas-invalidation repaint must each change zero pixels;
pane close must leave zero projected GPU bytes after `Window::drop_image`.

The probe applies typed scroll effects through
[[crates/scribe-client/src/terminal_image_scene.rs#CommittedImageScene#apply_grid_effect]],
then hands the resulting committed scene to the renderer. This exercises the
production common-placement logical envelope and clip for partially visible
Kitty and Sixel rasters. First and repeated scroll captures keep the original
source crop, destination extent, and nonzero Y offset while the stored clip
moves through the margins, proving no proportional recrop or rounding drift.
An off-margin placement keeps its resized envelope, X/Y offsets, mapping, and
sampled pixels through an unrelated margin scroll. Production-scene delete
counts prove physical effective-extent selectors, virtual-placement immunity,
hard-scope definition selection, unrelated unplaced-definition survival, and
explicit deletion of an unplaced image. Linux atlas
invalidation remains a device-recovery proxy, not a physical-device-loss claim.

## Native macOS Metal Validation

Native Metal evidence runs only on the sanctioned GPU-backed GitHub-hosted runner and never on a developer workstation.

`.github/workflows/native-macos-metal.yml` is a manual `workflow_dispatch` job
on ARM64 `macos-14-xlarge`. GitHub documents that runner class as carrying GPU
hardware acceleration. The workflow has read-only repository permission,
requires no secrets, builds release binaries, and invokes
`just native-macos-terminal-images`. The guarded recipe refuses every context
except GitHub Actions on the named macOS ARM64 runner, then requires executable
`tests/native-macos/terminal-images-metal.sh` before any Scribe runtime call.

A repository maintainer with GitHub write access owns dispatch, triage,
evidence review, and the release verdict. After the workflow is on the default
branch and the driver exists at the target ref, invoke:

```bash
gh workflow run native-macos-metal.yml --ref <release-candidate-ref>
```

The job records candidate SHA, ref, runner identity, OS, architecture, and
display/Metal metadata under `test-output/terminal-images/macos/`. The
driver receives that directory in `SCRIBE_NATIVE_MACOS_OUTPUT_DIR`; its merged
stdout/stderr is `run.log`. `actions/upload-artifact` archives the directory as
`native-macos-metal-<run-id>` for 14 days, even after a corpus failure. Download
it with `gh run download <run-id> --dir <destination>`.

The 120-minute job has no soft-failure path. Build, corpus, timeout,
missing-driver, and artifact-upload failures block platform-dependent GPUI work
and default-on release. Product failures require a fixing commit. A maintainer
may retry an Actions infrastructure failure only when both run URLs and the
rationale remain in release evidence. A green exact-candidate run and retained
artifact are both required; Linux Docker and package-only macOS jobs do not
substitute. See [[test#Sandbox limits#Host-only hardware and platforms]].

### Required Metal lifecycle assertions

The downstream native driver must run the same crop and lifecycle corpus on Metal plus a genuine recoverable device-loss phase.

The driver must record a Metal adapter, one upload for shared full/crop
placements, a green cropped quadrant, reusable texture space after
`drop_image`, three final-reference drops, unchanged pixels after recreation,
1-by-1 and 4096-by-1 uploads, and 4097-by-1 rejection with zero new
`RenderImage` objects. It must then induce one recoverable device loss through
a pinned test hook, observe GPUI context and atlas recreation, preserve source
identities, and require a zero-difference repaint.

Terminal placement rendering now exists, but the genuine Metal device-loss
hook and downstream native driver do not. The workflow remains fail-closed
until those native pieces land; the Linux atlas-clear proxy must not satisfy
the Metal device-loss assertion.
