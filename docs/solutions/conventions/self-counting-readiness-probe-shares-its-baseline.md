---
title: A readiness probe that emits the events it counts must rebaseline before the real assertion
date: 2026-08-19
component: tests/e2e/func/beads-board.sh, SGR mouse reporting
tags: [e2e, flaky-test, readiness, baseline, counter, mouse-reporting, sgr]
problem_type: convention
---

## Problem

`just e2e-func-beads-board` failed roughly one run in three:

```text
FAIL: owner-visible pane did not enable SGR mouse reporting
```

The other runs passed unchanged. Re-running was cheaper than investigating, so
the test trained readers to treat a real race as noise.

## Root cause

Two faults stacked.

The phase slept a fixed half second while DECSET crossed the shell, PTY, server
`Term`, and client `Term` asynchronously. Sometimes the client had parsed the
mode before the probe; sometimes not.

Less obvious: readiness and the later click assertion shared one counter
baseline. `PROBE_BEFORE` was sampled once, then the readiness probe moved the
pointer to generate the first SGR reports. The click assertion still compared
against `PROBE_BEFORE + 2`, so the readiness probe's own reports were credited
to the click. Whether the run passed depended on how many frames the probe
emitted.

A probe that emits the event it measures is not a neutral observer. Sharing its
baseline with a later assertion couples two checks invisibly.

## Fix

Landed in commit `2924bc3` for bead `scribe-xevt`.

`wait_for_mouse_report` at
`tests/e2e/func/beads-board.sh:553` polls for the first actual SGR frame — the
readiness signal the sleep was standing in for. After it returns,
`CLICK_BEFORE` is sampled at `:839`; the click assertions at `:843` and `:846`
therefore count only their own reports.

The two failure messages were split too, so a future failure names enablement
or click reporting rather than blaming both on the first phase.

## Prevention

When a wait and an assertion observe the same counter, ask whether the wait
produces the event the assertion measures. If yes, resample between them.

Prefer a bounded poll for the actual condition over a duration. It cannot be
tuned wrong and turns "flaky" into a specific timeout when the signal never
arrives.
