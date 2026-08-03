# Terminal Image Protocol Contract v1

This document freezes Scribe's first interoperable terminal-image subset and
the limits applied to untrusted PTY bytes. Implementations must match this
contract; expanding it requires a new contract version and fixtures.

Machine-readable values and the owned fixture inventory live in
`tests/e2e/fixtures/terminal-images/contract.json`. The Docker contract test
copies that exact file to `test-output/terminal-images/contract.json`.

## Primary sources

Facts were rechecked on 2026-08-03 against primary documentation and official
release metadata:

- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
  (including 4096-byte direct chunks, query ordering, placeholders, deletion,
  z-order, and terminal-action interaction)
- [DEC VT330/VT340 Sixel chapter](https://manx-docs.org/mirror/vt100.net/docs/vt3xx-gp/chapter14.html)
  (DCS format, bit order, palette, raster, repeat, and background semantics)
- [xterm control sequences, patch 410](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-Sixel-Graphics)
  and [xterm runtime behavior](https://invisible-island.net/xterm/manpage/xterm.html)
  (DA attribute 4 and the chosen `?80`/`?8452` behavior)
- [Yazi v26.5.6](https://github.com/sxyazi/yazi/releases/tag/v26.5.6),
  [Chafa 1.18.2](https://github.com/hpjansson/chafa/releases/tag/1.18.2),
  and [gnuplot 6.0.3](https://www.gnuplot.info/ReleaseNotes_6_0_3.html)

## Kitty v1 matrix

Kitty support is direct-inline and deliberately narrower than the full
protocol. Every omitted feature is unsupported, not silently approximated.

| Area | Supported exactly | Excluded exactly |
| --- | --- | --- |
| Framing | 7-bit APC `ESC _ G ... ESC \\`; comma-separated integer control pairs; one transfer in flight; `m=1/0`; each encoded chunk at most 4096 bytes and non-final chunks divisible by 4 | C1 APC, BEL termination, interleaved transfers, unknown duplicate keys, unterminated strings |
| Data | `t=d`; `f=24` RGB, `f=32` RGBA, `f=100` PNG; `o=z` RFC 1950 zlib; exact `s*v*channels` raw lengths | `t=f`, `t=t`, `t=s`, file paths, temporary files, shared memory, URLs, network fetches, generic format guessing |
| Actions | `a=t`, `a=T`, `a=p`, `a=q`, `a=d`; numeric image id `i` and placement id `p` | animation/frame actions `a=f/a/c`, image-number allocation `I`, usage hints `N`, protocol translation |
| Classic placement | Source crop `x/y/w/h`, cell span `c/r`, pixel offset `X/Y`, cursor policy `C=0/1`, signed 32-bit `z`; persistent exclusive logical-cell clip stays inside the placement envelope and preserves original mapping through viewport/margin changes; nonzero offsets extend the envelope one cell on their axis | Relative-parent `P/Q`, interaction/tooling extensions, destructive proportional recropping, undefined-overlap parity |
| Placeholder placement | `U=1`, `U+10EEEE`, official row/column/MSB diacritics, 8/24/32-bit image identity, underline placement id, specified left-cell inheritance; reserved background byte retained for IPC compatibility without adding image opacity | Horizontal-scroll inference beyond the official rules, treating rendered placeholder pixels as classic placements, or inventing a second placeholder alpha channel |
| Delete | Lowercase soft and uppercase data-freeing forms of `a/A`, `i/I`, `p/P`, `x/X`, `y/Y`, `z/Z`; physical extent selectors intersect the effective clip; virtual `U=1` placements survive every selector except explicit image id; data freeing selects definitions through placements actually removed, while explicit image id can target unplaced data; referenced and unrelated unplaced definitions survive; any delete aborts an incomplete transfer | `c/C`, `n/N`, `q/Q`, `r/R` selectors |
| Replies | APC `G` reply with echoed `i` and `p` when present; printable `OK` or stable error code; `q=0` normal, `q=1` suppresses success, `q=2` suppresses all replies | Client-generated replies and replies after a disabled capability claim |
| Layering | `z < -1073741824` below non-default cell backgrounds; other negative z above backgrounds but below glyphs; nonnegative z above glyphs; stable lower image id first for equal z | Claiming ordering for equal z and equal image id, which Kitty leaves undefined |
| Lifecycle | Same `i` retransmit replaces data and removes its placements; same `(i,p)` replaces placement; scrolling follows text; first margin scroll shifts the frozen anchor and stores `new envelope ∩ margin`, while later scrolls require `old envelope ∩ stored clip ∩ margin` and shift both; ED2 clears visible placements; RIS clears image state; 1049 alternate screen starts/ends empty; other text erases do not affect Kitty graphics | Animation persistence, cross-screen placement sharing, invented deletion behavior |

A valid `a=q` direct 1x1 RGB probe must receive its Kitty result before a
following DA1 reply. Query data is validated but never retained. Unsupported
direct formats/actions/transports return the typed Kitty error unless quiet;
when image support is disabled no Kitty reply is emitted, so discovery cannot
mistake policy-disabled Scribe for an enabled implementation.

## Sixel v1 matrix and chronology

Sixel follows current xterm behavior where DEC and xterm disagree. Scribe
does not claim literal VT340 parity for DECSDM.

| Area | Supported exactly | Excluded exactly |
| --- | --- | --- |
| Framing | 7-bit `ESC P ... ESC \\` and C1 `0x90 ... 0x9c`; `DCS P1;P2;P3 q`; CAN/SUB cancellation with adjacent-text recovery | BEL termination, unterminated/unbounded DCS, non-Sixel DCS interpretation |
| Raster | `?` through `~`, least-significant bit at top; `!Pn`; `"Pan;Pad;Ph;Pv`; `#Pc`, HLS `#Pc;1;H;L;S`, RGB `#Pc;2;R;G;B`; `$` carriage return and `-` sixel newline | ReGIS, vector/video, object ids, reusable placements, z-index, animation |
| Background | `P2=0` or `2` paints zero bits with current background; `P2=1` leaves zero-bit pixels unchanged; raster dimensions may establish background but never relax limits | Unbounded palettes, pixels, repeats, or raster growth |
| Discovery | DA1 preserves existing attributes and appends attribute `4` exactly once only when runtime-enabled | Terminal-brand or `$TERM` spoofing |
| Modes | xterm-compatible `?80l/h`, `?8452l/h`; private palettes per image, matching xterm's useful default | Literal DECSDM claim where DEC defines the inverse polarity |

Chronology is frozen as follows:

1. On `DCS`, capture cursor, margins, screen, `?80`, and `?8452`. Decode is
   ordered; later PTY bytes wait for commit, rejection, or cancellation.
2. With `?80l` (default), anchor at the current text cursor. Crop horizontally;
   scroll vertically at the bottom margin. On `ST`, put the cursor on the next
   complete text row at the original column. If `?8452h`, use the first column
   immediately right of the raster instead; `?8452l` keeps the original column.
3. With `?80h`, anchor at the page's upper-left, crop to the page, never scroll,
   and leave the text cursor unchanged; `?8452` has no cursor effect here.
4. Commit the anonymous raster and cursor/grid effect atomically, then process
   later text. The raster layer is above the default background and below
   non-default backgrounds and glyphs. Existing and later glyphs stay visible.
5. Cell erase, line erase, ED2, scroll, history trim, resize clipping, alternate
   screen destruction, and RIS affect Sixel raster in the same chronology as
   their grid effects. Resize clips without resampling. DECSTR preserves
   committed raster and resets modes; RIS clears it.

## Capability and reply lifecycle

Capability is session state owned by the server, independent of viewer count.

1. A session begins `text-only-unlatched`. A capable creator, or first capable
   viewer explicitly enabling images, latches the exact v1 subset.
2. Once latched, the server parses, replies, and retains bounded state while
   viewerless. Detach, replay, reconnect, and multiple viewers cannot duplicate
   a PTY reply. Replies use the server's internal PTY write path in byte order.
3. A viewer lacking the latched capability receives typed
   `capability_mismatch`; it is not attached with invisible graphics. Remote
   endpoints still require exact protocol versions.
4. Runtime availability is the intersection of compile-time support, decoder
   health, viewer capability, and the master switch. Kitty queries and DA1
   advertise only that effective subset.
5. Disabling the master switch stops new claims, cancels queued/active decode,
   clears partial and committed image state, omits DA attribute 4, emits no
   Kitty probe success, and returns the session to `text-only-unlatched` only
   after cleanup. Re-enable requires a new capable latch.
6. Handoff preserves latch, generation, committed state, and bounded partial
   framing only between capable versions. Unsupported downgrade is a typed
   mismatch, never a silent loss.

## Typed failure contract

Failures carry protocol/action plus safe dimensions, counts, and the limit
name. They never contain payload bytes, paths, URLs, decoded pixels, or base64.

| Category | Kitty result | Terminal effect |
| --- | --- | --- |
| `policy_disabled` | no discovery reply; `ECANCELED` only for a command already accepted before disable | cancel and clear at ordered boundary |
| `unsupported_protocol`, `unsupported_action`, `unsupported_transport` | `ENOSYS` or `EINVAL` unless quiet | consume bounded sequence; preserve adjacent text |
| `malformed_framing`, `malformed_control`, `malformed_payload`, `truncated_sequence`, `chunk_mismatch` | `EINVAL` unless quiet | abort partial transfer; recover at CAN/SUB/ST/bound |
| `invalid_dimensions`, `quota_exceeded`, `work_budget_exceeded` | `E2BIG` or `ENOSPC` unless quiet | reject before state/GPU allocation |
| `decode_deadline_exceeded`, `decode_cancelled` | `ETIMEDOUT` or `ECANCELED` unless quiet | discard incomplete result; stale completion cannot commit |
| `decode_failed`, `image_not_found` | `EIO` or `ENOENT` unless quiet | preserve protocol-defined prior committed image |
| `capability_mismatch` | not a PTY protocol reply | typed attach/handoff refusal |
| `renderer_unavailable` | not a PTY protocol reply | clear view resources; server state remains bounded |
| `evicted` | no unsolicited PTY reply | deterministic oldest unplaced definition first |

Diagnostics are payload-free and rate-limited by protocol, category, and
session. Applications remain responsible for textual fallback.

## ImageLimits v1

All byte values are binary bytes. Every add/multiply is checked before
allocation. Hitting a limit rejects the new command; limits never authorize
cross-session eviction.

| Field | Ceiling | Unit / enforcement boundary | Rationale |
| --- | ---: | --- | --- |
| `max_control_string_bytes` | 16,777,216 | bytes per APC/DCS, counted by framer before append | Bounds unchunked Sixel and malformed strings independently of decoded size. |
| `max_kitty_chunk_payload_bytes` | 4,096 | base64 bytes per APC payload, before append | Exact Kitty remote-client maximum. |
| `max_chunks_per_transfer` | 32,768 | APC chunks, before accepting next chunk | Allows a 64 MiB raw image at official chunk size while bounding empty/tiny chunk attacks. |
| `max_accumulated_encoded_bytes` | 89,478,488 | base64 bytes per Kitty transfer | Exactly `4 * ceil(64 MiB / 3)`. |
| `max_base64_decoded_bytes` | 67,108,864 | bytes per transfer, before decode-buffer growth | One maximum canonical RGBA image, including compressed input. |
| `max_inflated_bytes` | 67,108,864 | bytes emitted by zlib/PNG, checked during output | Stops decompression bombs before canonical allocation. |
| `max_width_pixels`, `max_height_pixels` | 4,096 each | pixels per axis, parsed before multiplication | Conservative cross-platform texture ceiling; later platform spike may lower, never raise, v1. |
| `max_pixels` | 16,777,216 | checked `width * height` pixels | Full 4096 square and an independent multiplication guard. |
| `max_canonical_rgba_bytes` | 67,108,864 | checked `pixels * 4` bytes per definition | Bounds retained canonical CPU data and upload source. |
| `max_images_per_session` | 128 | committed definitions | Bounds identifier/state overhead even for tiny images. |
| `max_placements_per_session` | 1,024 | classic, virtual, and Sixel placements | Bounds layout, scroll, delete, replay, and paint work. |
| `max_session_retained_cpu_bytes` | 134,217,728 | canonical definitions plus partial encoded/decoded buffers | Two maximum images or a useful larger small-image set per pane. |
| `max_view_projected_gpu_bytes` | 268,435,456 | RGBA texture plus one upload/crop staging estimate, before IPC/upload | Covers two copies of the CPU cap while preventing a view from reaching GPUI unbounded. |
| `max_process_retained_bytes` | 536,870,912 | all sessions' image CPU, queue, decoder work, and projected active upload bytes | Hard aggregate boundary across panes and viewers. |
| `max_concurrent_decodes` | 2 | process worker permits, acquired before decode | Prevents CPU/memory fan-out without serializing all panes. |
| `max_decode_queue_depth` | 8 | process queued commands, checked before enqueue | Bounds ordered-barrier waiters and scheduler metadata. |
| `max_decode_queue_bytes` | 134,217,728 | encoded bytes charged before enqueue | Queue depth alone cannot bound variable-sized commands. |
| `max_work_units_per_command` | 134,217,728 | units; one per input byte examined, output byte emitted, or pixel write, charged cumulatively | Bounds repeat, inflate, palette, conversion, and gradual-growth CPU work. |
| `max_queue_wait_ms` | 1,000 | monotonic milliseconds from enqueue to worker start | Rejects a blocked ordered barrier before it stalls a session indefinitely. |
| `max_decode_ms` | 2,000 | monotonic milliseconds from worker start through normalized result, checked at least every 4,096 work units | Bounds wall time while cooperative work accounting handles fast loops. |
| `max_replay_chunk_bytes` | 1,048,576 | canonical bytes per typed replay/handoff chunk | Keeps image replay below existing IPC framing and queue spikes. |

The 64 MiB per-image ceiling is intentional: it is the exact RGBA size of a
4096-square image. Session, view, and process caps separately account for
copies and concurrency. These are security ceilings, not performance targets.

## Pinned application and fixture corpus

The application pins are release-gate inputs, not dependencies installed by
this contract task.

| Application | Version | Required path |
| --- | --- | --- |
| Yazi | `v26.5.6` (`aa526434f00bb44e2e902d9a4ac5f810da1018b9`) | Unknown terminal receives a successful direct Kitty query and selects `KgpOld`; direct PTY and SSH; no `$TERM`/`TERM_PROGRAM` spoofing. Owned fixtures, not Yazi recognition, prove placeholders. |
| Chafa | `1.18.2` | `--format kitty --probe off` and `--format sixels --probe off`; direct PTY and SSH. |
| gnuplot | `6.0.3` | `set terminal sixelgd`; direct PTY and SSH with the build's terminal list verified first. |

The owned manifest covers query ordering; raw RGB; chunked zlib RGBA; PNG;
Unicode placeholders; delete forms; 7-bit and C1 Sixel; mode chronology; and
CAN/SUB malformed recovery. Fixtures are ASCII hex so review and transport do
not alter control bytes. Their exact paths and expected outcomes are frozen in
the machine-readable contract and `fixtures.tsv`.
