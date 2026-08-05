# Terminal Image Performance and Resource Review

Clarification 7C decides terminal-image performance acceptance by recorded
measurement and human judgement rather than by a numeric threshold. This is
that judgement, taken against the measurements the two named commands
produced.

## Scope and rationale

Numeric performance goals are explicitly **inapplicable** for v1 under
Constitution principle 4, which permits marking them so as long as hot-path
behavior is still verified by a named command. Both commands below therefore
gate on nothing they measure; every recorded value exists to be read, not
compared against an invented ceiling. No measurement in this review may be
turned into a frozen limit.

The exact numeric `ImageLimits` ceilings are a different thing entirely. They
bound untrusted payload handling, they are frozen in
[`contract.md`](contract.md), and both scripts assert them. Nothing in this
review relaxes them.

## Named commands

```bash
just e2e-func terminal-images-performance.sh
just e2e-visual terminal-images-frame-stability.sh
```

Both run inside the authorized Docker harness. They write
`test-output/terminal-images/performance.json` and
`test-output/terminal-images/linux/client/frame-stability.json`; the captures
and server log beside them are the raw evidence. Every number below is copied
from those two manifests.

## Server measurements

One session, one server, measured with the image path off and then on. The
"off" side is the master switch disabled, which removes the graphics path from
the session entirely; the "on" side is the same session, latched, with a
sixteen-image scene resident.

| Measurement | Text-only | Images enabled |
| --- | --- | --- |
| 2,124,747 B text burst | 137 ms | 125 ms |
| Input round trip, median of 20 | 20 ms | 20 ms |
| Input round trip, min / max | 20 / 21 ms | 19 / 24 ms |
| Server CPU over the same work | 130 ms | 120 ms |
| Server resident set | 39,992 kB | 47,736 kB |

Fixed multi-image workload: sixteen 256x256 direct RGB transfers, each 64
chunks at the frozen 4,096-byte ceiling, 4,194,304 canonical RGBA bytes in
total.

| Measurement | Value |
| --- | --- |
| First image, PTY write to committed decode | 74 ms |
| Remaining fifteen, back to back | 306 ms (20 ms per image) |
| All sixteen retransmitted as replacements | 298 ms |
| Resident growth across the workload | 5,824 kB |
| Resident growth across the replacement pass | 2,560 kB |
| Peak resident set | 54,840 kB |
| Placements after workload / after replacement | 16 / 16 |
| Evictions required | none |

## Client measurements

The shipped GPUI client on a live pane, six captured frames per idle sample.

| Measurement | Value |
| --- | --- |
| Transmission to first painted frame | 454 ms |
| Idle frame-to-frame difference, text only | 0..1,672 px |
| Idle frame-to-frame difference, scene resident | 0..1,744 px |
| Painted image pixels across six idle frames | 10,955..11,088 |
| Scroll repaint | 2,079 ms |
| Projected GPU bytes, eight distinct sources | 36,864 (4,608 each) |
| Client resident set, start to end | 182,128 kB to 186,424 kB |

## Conclusion — no material regression

Signed judgement, on the evidence above:

1. **Text throughput does not materially regress.** The image-enabled burst
   was faster than the text-only one (125 ms against 137 ms) for identical
   bytes. The two are within run-to-run noise of each other, which is the
   point: enabling the graphics path did not put a measurable cost in front of
   ordinary output.
2. **Input latency does not materially regress.** Both medians are 20 ms and
   both are dominated by the harness CLI's own process spawn, which is a
   constant across the two phases.
3. **CPU does not materially regress.** 120 ms against 130 ms of server CPU
   for the same burst and the same twenty round trips.
4. **Frame stability does not materially regress.** The idle frame difference
   with a scene resident (0..1,744 px) matches the text-only baseline
   (0..1,672 px); both are the window's own cursor blink. The painted image
   varied by 133 px across six frames — antialiased box edges, not a scene
   dropping in and out — and every frame kept the image.
5. **Decode and upload latency are proportionate.** 74 ms for the first
   256x256 transfer including chunk accumulation, 20 ms per image thereafter,
   454 ms from a shell writing bytes to the first frame that shows them. A
   user watching an image appear sees it appear.
6. **Retention is bounded and behaves.** 5,824 kB of resident growth for
   4,194,304 bytes of canonical RGBA, and re-transmitting all sixteen
   identifiers added 2,560 kB rather than another whole scene, so replacement
   releases what it replaces. The view charged 36,864 projected GPU bytes for
   eight sources against a 268,435,456-byte ceiling; nothing came close to
   needing eviction.

## Security ceilings remain enforced

Measured after the whole workload had run, not before it:

- A Kitty transmit declaring 4,097 pixels of width — one past the frozen
  `max_width_pixels` — was refused, retained no placement, and left the
  session usable.
- The view's projected GPU charge is asserted against the frozen
  `max_view_projected_gpu_bytes`.
- The server did not panic at any point in either pass.

These are the assertions in both scripts. They are security limits, and this
review does not touch them.

## Defect found during the pass

The client pass records that a single committed read carrying eight distinct
image definitions delivered three of them to the view, while the same eight
delivered one per committed read all arrived. The server holds all eight
canonically in both cases. This is a convergence correctness defect, filed as
`scribe-aq1.27`, and it is **not** part of the no-material-regression
conclusion above: it changes what a viewer sees, not what performance costs.
The measurement script records it (`single_burst_sources_uploaded` against
`single_burst_sources_transmitted`) without gating on it, so the reproduction
stays in the corpus until the bug is fixed.

## Reviewer of record

| Field | Value |
| --- | --- |
| Reviewer | Sharaf Nassar, repository maintainer |
| Date | 2026-08-05 |
| Evidence | `test-output/terminal-images/performance.json`, `test-output/terminal-images/linux/client/frame-stability.json` |
| Conclusion | No material performance or resource regression from terminal image support on Linux/Docker |
| Outstanding | `scribe-aq1.27` (correctness, not performance); native macOS Metal parity is reviewed separately under its own gate |

Release approval remains the maintainer's, and this review covers the Linux
Docker surface only. The macOS numbers, if any are ever wanted, come from the
sanctioned native workflow and not from this pass.
