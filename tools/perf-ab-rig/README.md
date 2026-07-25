# Perf A/B rig

Launch-blocking performance comparison for the GPUI client rebuild (beads
`scribe-38e.41`, `scribe-38e.51`). It measures the five Clarification-Q3 metrics
for the new GPUI client, measures the comparative ones on the old client with
the same instrumentation, and writes a markdown report with a per-metric
pass/fail verdict.

## Metrics and thresholds (Clarification Q3)

| Metric | Threshold | How it is measured |
|---|---|---|
| Startup to first frame | `<= 500 ms` absolute (also gates splash deletion) | GPUI first-frame marker (`SCRIBE_GPUI_STARTUP_TIMING`) |
| Input latency | no worse than old client | shared probe: median key to PTY-echo round trip |
| cat-firehose throughput | no worse than old client | shared probe: bytes drained per second while `cat`ting a 32 MiB file |
| Memory at 10 tabs | `<= old + 20%` | `/proc/<pid>/status` `VmRSS` once the probe reports 10 sessions |
| Scroll | sustained 60 fps, `< 1%` dropped | shared probe: frame count and frame-gap drop accounting over a driven scroll |

"No worse than the old client" is enforced with a 10% run-to-run noise
allowance, which is the repeatability of these measurements on a loaded
desktop, not extra headroom.

## Shared runtime probe

Both clients link `crates/scribe-common/src/perf_probe.rs`. It activates only
when `SCRIBE_PERF_PROBE` names a report path, so normal runs pay nothing. The
report is a flat `key=value` file rewritten at most every 200 ms with cumulative
counters plus an `uptime_ms` stamp:

    pid, uptime_ms, frames, dropped_frames, pty_bytes, input_samples,
    input_latency_p50_ms, input_latency_mean_ms, sessions, session_ids,
    focused_session

Because the counters are cumulative, the rig derives a per-workload number by
reading the file before and after a workload and dividing the deltas, so the
client needs no window bookkeeping.

## Modes

- **assess** (default): generates the current-state report from the committed
  baseline plus a static capability check. Never launches a GUI and never
  touches the live server, so it is safe in CI or a shared session. Metrics that
  need a live client are reported `NOT-MEASURED`.
- **`--live`**: the launch-gate mode. Launches each target client on the same
  machine/session, drives every workload with `xdotool`, captures the numbers,
  and enforces the thresholds. It attaches to the already-running server and
  **never restarts it**.

## Live-mode safety

Every typing workload runs in a tab the rig opened itself: it sends the
`new_tab` binding, then waits for the probe to report a session id that was
**not** in the list at launch and that is now focused. If that never happens the
workload aborts instead of typing into a pane that was already open. The rig
closes the tabs it opened afterwards, and only ever sends `exit` to a session it
watched itself create.

## Driving input

Three `xdotool` details are load-bearing, each established empirically after it
silently zeroed a metric:

- **Enter is always its own key event.** `xdotool type` does not deliver a
  trailing newline as `Return` to either client — the command line echoes and
  then sits there unexecuted — so every command goes out as `type` followed by
  `key Return`. Without this the firehose and scroll workloads silently measure
  nothing.
- **The pager is advanced with `space`, not PageDown.** `space` is `less`'s
  canonical page-forward key and a plain printable character both clients
  encode through their simplest path, so the scroll metric measures paint
  rather than key encoding. This started as a workaround: a synthetic `Next`
  was dropped between the X event and the PTY on the GPUI client, scoring the
  workload as "the client painted nothing". `scribe-38e.84` fixed that by
  wiring the ported encoder into the live key path, but `space` stays the drive
  key because it depends on the least machinery.
- **Key delivery falls back.** The rig prefers window-targeted synthetic events
  (`xdotool key --window`) because a stray keystroke then cannot escape into
  another application. A toolkit that reads keys through XInput2 ignores those
  events entirely, so if the new-tab binding produces no tab the rig retries
  with XTEST (`xdotool key`, which needs the client window to hold X input
  focus) and keeps whichever mode worked.

Live mode therefore needs a session where the client actually receives
synthetic keys. A bare `Xvfb` with no window manager is not sufficient for the
old client: it advertises no `_NET_ACTIVE_WINDOW`, and the winit-based client
receives neither delivery mode there, so its half of the A/B reports
`NOT-MEASURED`. Run the gate on a window-managed display.

## Usage

```bash
# Safe current-state report (default output path under specs/016-...):
tools/perf-ab-rig/run-perf-ab.sh

# Launch gate: full A/B against both client binaries, recording the old
# client's numbers into perf-baseline.md as it goes:
tools/perf-ab-rig/run-perf-ab.sh --live \
  --new-client target/release/scribe-client-gpui \
  --old-client /usr/bin/scribe-client --record-baseline
```

Workload sizing is tunable: `--samples N` (latency keystrokes),
`--firehose-mib N`, `--tabs N`.

## Verdicts

`PASS` requires all five metrics measured and inside their thresholds. `FAIL` on
any metric fails the gate and reopens the perf bead. `INCOMPLETE` means a metric
could not be captured at all: no display, no server, a missing binary, or a
comparative metric with no committed baseline (`NO-BASELINE`).
