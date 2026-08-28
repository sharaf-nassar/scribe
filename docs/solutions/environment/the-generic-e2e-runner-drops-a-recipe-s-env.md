---
title: The generic e2e runner drops a recipe's env, so the rig silently changes
date: 2026-08-28
component: justfile, docker/entrypoint-visual.sh, tests/e2e/visual
tags: [e2e, just, recipes, shared-pane, rig, false-positive, test-output]
problem_type: environment
---

## Problem

`tests/e2e/visual/mouse-reporting.sh` was reported as a pre-existing failure:

```text
PHASE 2 FAIL: the display offset stayed at the live bottom:
terminal scrollback moved session=session-4d3619af scroll=Delta(3)
    moved=false offset=0 pin_rows=0
```

It reproduced on several commits, which looked like solid evidence of a real
product bug in scroll handling. It was not. The suite had been invoked as:

```bash
just e2e-visual mouse-reporting.sh      # generic runner
```

instead of its own recipe:

```bash
just e2e-visual-mouse-reporting          # justfile:560
```

## Root cause

Per-script recipes carry environment the generic runner knows nothing about.
`justfile:560-561` passes `-e SCRIBE_SHARED_PANE=1`; the generic `e2e-visual`
recipe passes none of it.

`SCRIBE_SHARED_PANE` selects an entirely different rig in
`docker/entrypoint-visual.sh:445-472`. With it, the harness session is created
*first* and the client is handed the daemon's window id so both attach to the
same pane. Without it, the `else` branch runs: the client boots first and
bootstraps **its own** shell session, and `SESSION` is created afterwards, on a
different window.

The server log shows exactly that split brain:

```text
client identified via Hello window_id=win-5e1bd9a1   <- daemon
client identified via Hello window_id=win-d0ac2f6e   <- client, different window
created new PTY session self.session_id=session-4d3619af   <- client's own
created new PTY session self.session_id=session-e3ad6891   <- $SESSION, later
```

So phase 1 seeded 200 rows into `$SESSION` while the client displayed a
different, empty session. `moved=false offset=0` was correct: that pane had no
scrollback. The rig even warns, but only advisorily —
`wait_for_log "attaching to session" 20 || echo "WARNING: ..."` — and the
script's phase 0 gate only counts lit pixels, then *claims* attachment it never
checked. A bare prompt cleared the 1500px threshold at 2118px.

A second trap compounded it. `test-output/` is a persistent bind mount shared by
every recipe, so `share-wire.jsonl` still held a previous suite's frames. Reading
it produced a `MoveWorkspace` that belonged to `tab-drag-cross-region.sh`, with
session ids from that run, and nearly supported a completely wrong conclusion.

## Fix

Use the script's own recipe. Find it with the contract map in `justfile` (near
`:750-800`), which pairs every executable script to its recipe:

```text
'visual/mouse-reporting.sh|e2e-visual-mouse-reporting'
```

Under the correct recipe phases 0-9 pass immediately, and the run surfaces the
*real* defect at phase 10, which was a genuine product bug in pane focus fixed
separately as `scribe-d4wy` (commit `11458e9`).

Clear the shared output directory before trusting any artifact from it:

```bash
rm -f test-output/share-wire.jsonl test-output/client.log test-output/server.log
```

## Rule

A visual script's recipe is part of its contract, not a convenience wrapper.
Before believing an E2E failure, confirm the invocation matches the contract map
and that `test-output/` artifacts belong to this run — an unexpected message type
or an unfamiliar session id in the wire tap means the record is stale. A phase
that asserts pixels while its message claims attachment will happily pass on the
wrong pane.
