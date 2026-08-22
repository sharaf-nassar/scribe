---
title: Host build load makes bd miss the server's 5s deadline and looks like an E2E phase regression
date: 2026-08-21
component: tests/e2e/func/beads-board.sh, crates/scribe-server/src/beads_board.rs
tags: [e2e, flaky-test, attribution, beads, bd, timeout, host-load]
problem_type: environment
---

## Problem

`just e2e-func-beads-board` failed in the keyboard-move phase:

```text
FAIL: e2e-ready returned no applied write result
```

That phase had been green for every prior run of the suite, and the worker's own
change touched only the last phase in the file. The next run failed instead in
the detail phase, hundreds of lines earlier:

```text
FAIL: real server sent no matching detail response
```

Two different phases, neither related, both looking like a regression somewhere
upstream of the change under test.

## Root cause

Neither was a phase bug. Both failures reduce to one server-side deadline.
`COMMAND_TIMEOUT` in `crates/scribe-server/src/beads_board.rs` gives every `bd`
read five seconds of wall time. Whatever the phase was asserting, the wire and
the server log say the same thing:

```text
BeadsIssueWriteResult { "failed": { "reason": "bd board query timed out" } }
WARN Beads issue-detail query failed ... error=bd board query timed out
INFO Beads issue write finished ... outcome="failed" elapsed_ms=5251
```

`elapsed_ms=5251` is the 5s timeout, not slow work — a write that succeeds takes
about 1000 ms end to end. The guard read inside `write_issue` is an ordinary
`bd show`; timed in the same image on the same host it runs in 450–700 ms. It
needs a roughly tenfold stall to blow the deadline.

The stall came from outside the container. Three sibling worktrees were running
`cargo build --release` and `cargo clippy` (thin LTO, one `rustc` at 1400% CPU),
putting the host at a load average near 35. The E2E container competes for the
same cores, and the visual image renders GPUI through Lavapipe — software
rasterisation, entirely on CPU. Under that, `bd` misses a wall-clock deadline
that has a tenfold margin when the host is quiet. The same suite passed twice in
a row at load average ~13 with no change to any phase it had failed in.

## The trap

The failure surfaces as whatever assertion happened to be waiting, so it names
an innocent phase — and a different one each run. Chasing it as a phase bug
means "fixing" landed, previously green work. Two of this bead family's attempts
went that way.

## What to do

Read the failure signature before the phase. `test-output/` is bind-mounted, and
the entrypoint appends to it, so read those files rather than the streamed docker
output, which can be cut before the final lines flush:

```bash
grep 'timed out' test-output/server.log
python3 -c 'import json;[print(json.loads(l)["message"]) for l in open("test-output/share-wire.jsonl")]' | grep -i timed
```

If `bd board query timed out` appears, check `uptime` and `ps -eo pcpu,args
--sort=-pcpu | head` before touching a phase. A load average near the core count,
with `rustc` at the top, is the answer. Wait for the sibling builds to drain and
rerun; do not weaken the phase that reported it.

## Prevention

A failure a phase cannot observe is not that phase's failure. When an E2E
assertion is a bounded poll for something the server produces, the poll timing
out tells you only that the server did not produce it — the reason is in the
server log and the wire tap, and it may be nothing to do with the assertion's
subject. Confirm the signature is stable across runs before attributing it, the
same discipline
`visual-e2e-recipes-can-report-on-a-stale-image.md` in this directory asks for.
