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
