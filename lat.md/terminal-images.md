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
background alpha color.

Kitty delete selectors are exactly `a/A`, `i/I`, `p/P`, `x/X`, `y/Y`, and
`z/Z`. Any delete aborts an incomplete transfer. `q=0` permits success and
failure replies, `q=1` suppresses success, and `q=2` suppresses all replies.
Retransmitting an image id replaces its data and placements; reusing an
image/placement pair replaces that placement.

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
scroll/cursor behavior, and bounded placeholder metadata. Typed grid effects
bind image-driven cursor, scroll, erase, resize, screen, and reset consequences
to the same ordered client operation stream.

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

[[third_party/icy-sixel-decoder/src/lib.rs#DecodeLimits]] vendors the decoder
concepts from `icy_sixel 0.5.0`, crates.io SHA-256
`85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5`,
at upstream revision `998cbb2c6d8ed5272f9cc4702a4660778972bf3f`.
`README.md`, `LICENSE-MIT`, and `LICENSE-APACHE` in that directory retain the
source URL, exact license texts, fork delta, excluded code, and Scribe
maintainer ownership for CVE and upstream-release review.

`DecodeLimits` carries the frozen 4096-axis, 16,777,216-pixel, 67,108,864-byte
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
Sixel libraries are absent. `DEPENDENCY-TREE.txt` records the dependency-free
normal tree and its `scribe-test` reverse edge.

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
types, checks maximum and maximum-plus-one bounds, and writes
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
