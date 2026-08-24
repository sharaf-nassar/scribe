---
title: A gated tracing probe also needs its target enabled in RUST_LOG
date: 2026-08-24
component: visual E2E, tracing, workspace drag latency probe
tags: [tracing, rust-log, e2e, performance, probe, workspace-drag]
problem_type: environment
---

# A gated tracing probe also needs its target enabled in RUST_LOG

## Problem

The workspace-drag latency probe opened a focused two-region window and scanned
the pill hit area, but reported no arm event and zero `input_to_paint_us`
samples. The first diagnosis blamed pointer coordinates.

## Root cause

The probe events use the explicit target `scribe::drag_probe`
(`crates/scribe-client/src/main.rs:7476-7492` and
`crates/scribe-client/src/workspace_drag.rs:562-569`). The visual entrypoint's
default filter enables `scribe_server=info,scribe_client=info` only
(`docker/entrypoint-visual.sh:359-360`). Enabling `SCRIBE_DRAG_PROBE=1` made the
instrumentation execute, but EnvFilter still discarded every event from its
separate target.

## Fix

Relaunch the measured client with both switches:

```bash
SCRIBE_DRAG_PROBE=1 RUST_LOG='scribe_client=info,scribe::drag_probe=info' \
  scribe-client
```

The checked-in probe does this at
`tests/e2e/visual/drag-latency-probe.sh:42-47`. The next release X11/Lavapipe
run found the first pill target, collected 283 samples, and measured p95
12.080 ms against the 16.7 ms budget.

Landed for bead `scribe-07xb.10` in squash commit
`1f1a2e88b769a8d87289374984caeba00712b37f`.

## Prevention

For every opt-in tracing probe, verify two independent gates before debugging
the measured behavior:

1. the code-path switch is enabled in the process environment;
2. the tracing target is admitted by the active `RUST_LOG` filter.

Log or assert both at probe startup. A zero-sample result with a custom target
is a logging-pipeline failure until proven otherwise, not evidence that input
never reached the application.
