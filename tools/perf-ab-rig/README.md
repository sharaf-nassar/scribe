# Perf A/B rig

Launch-blocking performance comparison for the GPUI client rebuild
(bead `scribe-38e.41`). It measures the five Clarification-Q3 metrics for the
new GPUI client and compares them against the recorded old-client baselines,
then writes a markdown report with a per-metric pass/fail verdict.

## Metrics and thresholds (Clarification Q3)

| Metric | Threshold | Old baseline source |
|---|---|---|
| Startup to first frame | `<= 500 ms` absolute (also gates splash deletion) | `perf-baseline.md` (190 ms) |
| Input latency | no worse than old client | live capture at gate |
| cat-firehose throughput | no worse than old client | live capture at gate |
| Memory at 10 tabs | `<= old + 20%` | live capture at gate |
| Scroll | sustained 60 fps, `< 1%` dropped | live capture at gate |

## Modes

- **assess** (default): generates the current-state report from the committed
  baseline plus a static capability check. Never launches a GUI and never
  touches the live server, so it is safe in CI or a shared session. Metrics
  that need a feature-complete client are reported `DEFERRED` with the exact
  live method the launch gate uses.
- **`--live`**: the launch-gate mode. Launches the target client on the same
  machine/session, drives each workload, captures the numbers, and enforces the
  thresholds. It attaches to the already-running server and **never restarts
  it**.

## Usage

```bash
# Safe current-state report (default output path under specs/016-...):
tools/perf-ab-rig/run-perf-ab.sh

# Launch gate: full A/B against a built client binary:
tools/perf-ab-rig/run-perf-ab.sh --live \
  --new-client target/release/scribe-client-gpui
```

## Startup instrumentation

The GPUI client writes a machine-readable first-frame marker
(`first_frame_ms=<n>`) to the file named by the `SCRIBE_GPUI_STARTUP_TIMING`
environment variable, and only when that variable holds a non-empty path. The
timer starts at the top of `main` and fires on the first painted frame,
mirroring the old client's `init_gpu_and_terminal_done` measurement that
produced the recorded baseline. The rig points the variable at a temp file
automatically for the startup measurement.

## Current status

The GPUI client is a display-only scaffold spike: it has no stable input
encoder with echo instrumentation, no multi-tab support, and no scroll with a
frame counter. Only startup-to-first-frame is measurable today, and only under
`--live` on a machine with a display and a running server. The remaining four
metrics are `DEFERRED` until the client is feature-complete; the launch gate
(`scribe-38e.42`) re-runs this rig with `--live` at cutover and enforces every
threshold.
