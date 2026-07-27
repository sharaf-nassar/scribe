# Perf A/B rig

Launch-blocking performance comparison for the GPUI client rebuild (beads
`scribe-38e.41`, `scribe-38e.51`). It measures the five Clarification-Q3 metrics
for the new GPUI client, measures the comparative ones on the old client with
the same instrumentation, and writes a markdown report with a per-metric
pass/fail verdict.

## Metrics and thresholds (Clarification Q3, startup re-scoped 2026-07-24)

| Metric | Threshold | How it is measured |
|---|---|---|
| Startup to first frame | no worse than old client end-to-end (also gates splash deletion) | new: GPUI first-frame marker (`SCRIBE_GPUI_STARTUP_TIMING`); old: wall-clock from its first startup-timing log line to `init_gpu_and_terminal_done`; median of 3 each |
| Input latency | no worse than old client | shared probe: median key to PTY-echo round trip |
| cat-firehose throughput | no worse than old client | shared probe: bytes drained per second while `cat`ting a 32 MiB file |
| Memory at 10 tabs | `<= old + 20%` | `/proc/<pid>/status` `VmRSS` once the probe reports 10 sessions |
| Scroll | sustained 60 fps, `< 1%` dropped | shared probe: frame count and frame-gap drop accounting while an unpaced writer scrolls the grid; measured on both clients |

"No worse than the old client" is enforced with a 10% run-to-run noise
allowance, which is the repeatability of these measurements on a loaded
desktop, not extra headroom. Scroll is the one absolute threshold, so the old
client's number there is attribution rather than a comparison input.

The startup threshold was re-scoped on 2026-07-24 from the original
`<= 500 ms` absolute budget (spec.md "Q3 re-scope" records the decision).
The absolute number was anchored to a 190 ms "old client baseline" that
turned out to be a phase-scoped GPU-init timer, not process-start to first
frame, and is unreproducible; measured end-to-end on the reference host the
old client takes 3.0-5.5 s while the GPU driver bring-up floor alone exceeds
500 ms (beads scribe-38e.50 / scribe-38e.83 carry the measurements). The two
clients are therefore compared end-to-end with the same definition, like
every other comparative metric. The old client's phase-scoped `total_ms`
value on the `init_gpu_and_terminal_done` line is never compared against the
GPUI first-frame marker.

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

The report is only rewritten when the client paints or drains PTY bytes, which
makes `uptime_ms` a stamp of the client's last activity rather than of now. A
before-snapshot taken while the client is quiet therefore names a moment
arbitrarily far in the past, and the window derived from it is that much too
long. The scroll workload is the one that waits for its workload to settle, so
it waits for a rewrite before it snapshots; see
[The scroll metric measures the renderer, not the rig](#the-scroll-metric-measures-the-renderer-not-the-rig).

## Modes

- **assess** (default): generates the current-state report from the committed
  baseline plus a static capability check. Never launches a GUI and never
  touches the live server, so it is safe in CI or a shared session. Metrics that
  need a live client are reported `NOT-MEASURED`.
- **`--live`**: the launch-gate mode. Launches each target client on the same
  machine/session, drives every workload with `xdotool`, captures the numbers,
  and enforces the thresholds. It attaches to the already-running server and
  **never restarts it**.

`--startup-only` and `--scroll-only` each limit a `--live` run to one metric;
the others report `NOT-MEASURED`. They are the fast iteration loops for the
startup and scroll perf beads.

## Live mode never drives the stable server

A live run seeds sessions, opens ten tabs, types shell commands into them and
types `exit` to close them again. Against the stable install all of that lands
in whatever terminal the developer is actually using, so the rig runs against
the isolated `scribe-dev` install instead, and `--live` refuses to start when
that server is not up.

Scribe derives its whole runtime identity from the running executable's file
stem (`AppIdentity::detect_from_path` in `crates/scribe-common/src/app.rs`): a
binary named `scribe-dev` uses `/run/user/<uid>/scribe-dev/server.sock`,
`~/.config/scribe-dev` and `~/.local/state/scribe-dev`; anything else uses the
stable slug. There is no environment override, so the rig copies every binary it
launches — both clients and `scribe-test` — into its work directory under that
name and runs the copies. The copies are byte-identical to the binaries passed
in, so the numbers still describe the binaries under test.

Install and start the dev server once with `just install-dev`.

## Live-mode prerequisites

A `--live` run has two hard prerequisites that the rig checks before it launches
anything, and fails on rather than working around:

- **A usable `scribe-test`.** The rig seeds one detached session for the client
  to attach to. A client that claims an empty window has no workspace, and both
  clients then refuse to open a tab, so every workload metric would be
  unmeasurable. Pass `--scribe-test target/release/scribe-test` when the helper
  is not on `PATH`.
- **Client binaries that carry the shared probe.** The probe is armed through
  `SCRIBE_PERF_PROBE`, so the rig checks for that string inside each client
  binary. The installed `/usr/bin/scribe-client` predates the probe and never
  writes a report: it starts and shows a window, but every rig wait keyed off
  the report file burns its full timeout. Point `--old-client` at
  `target/release/scribe-client`.

Both used to degrade silently, and both surfaced as `NO-BASELINE` on the three
workload metrics — which reads as "no baselines have been captured yet" rather
than "this run was handed inputs it cannot measure". That misdiagnosis cost two
full gate runs during `scribe-38e.42`, so they are now fatal (bead
`scribe-38e.97`).

`--startup-only` is the one exception on the probe check. Metric 1 has a
documented fallback to the startup-log method for a binary without the probe
key, and such a run opens no tabs, so it needs neither the probe nor
`scribe-test`; a probe-less binary is logged there instead of rejected.

An environment that cannot host a client at all — no `DISPLAY`, no `xdotool`, no
running server — is *not* fatal. That stays a `NOT-MEASURED` report, because it
describes the machine rather than the run's arguments.

## Live-mode safety

Every typing workload runs in a tab the rig opened itself: it sends the
`new_tab` binding, then waits for the probe to report a session id that was
**not** in the list at launch and that is now focused. If that never happens the
workload aborts instead of typing into a pane that was already open. The rig
closes the tabs it opened afterwards, and only ever sends `exit` to a session it
watched itself create.

## Driving input

Three `xdotool` details are load-bearing, each established empirically after it
silently corrupted a metric rather than failing:

- **Enter is always its own key event.** `xdotool type` does not deliver a
  trailing newline as `Return` to either client — the command line echoes and
  then sits there unexecuted — so every command goes out as `type` followed by
  `key Return`. Without this the firehose and scroll workloads silently measure
  nothing.
- **Synthetic keys cannot pace a frame-rate workload.** `xdotool` delivers about
  one key per 21 ms on the reference host regardless of `--repeat-delay`, so any
  metric whose unit of work is one keystroke is capped at ~47 Hz. That is why
  the scroll workload no longer types at all; see
  [The scroll metric measures the renderer, not the rig](#the-scroll-metric-measures-the-renderer-not-the-rig).
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

## The scroll metric measures the renderer, not the rig

Metric 5 used to run on the new client only and read `29 fps, 14% dropped`
against a 60 fps target, with no way to tell a paint-path regression from a
target the workload could not reach. Bead `scribe-38e.91` added the old-client
arm; the comparison showed the old client pinned at the same ceiling, and both
halves of the shortfall turned out to be in the rig.

**The window was inflated by the client's own idle.** It is derived from the
probe's `uptime_ms` stamps, and the report is only rewritten when the client
paints or drains PTY bytes — so after the wait for the workload to settle, a
client that had gone quiet still carried the stamp from whenever it last drew.
The frame counts were right and the elapsed time was seconds too long, which
understated every fps by roughly 45%, and understated it *most* for the client
that idles best: the GPUI client stops painting entirely and its report went
4.1 s stale, while the old client kept draining bytes and its went 0.5 s stale.
The A/B read that difference as a paint-path regression. The rig now waits for
the report to be rewritten before it opens the window, so both edges name a
moment the client was busy.

**The workload could not ask for 60 fps.** It paged `less` with synthetic
`space` events, and each page-forward produces exactly one repaint — so the
measured frame rate was the rate at which `xdotool` could deliver synthetic
keys. That is 21 ms per key on the reference host (60 keys with
`--repeat-delay 8` take 1262 ms, not 480 ms), a hard ceiling of ~47 fps against
a 60 fps target, and *both* clients sat on it: a steady 21 ms between painted
frames at 5-10% CPU. Driving the scroll from an unpaced writer inside the pane
instead removes the rig from the loop, and the GPUI client then sustains 60 fps
with no dropped frames.

The threshold never needed re-scoping — see `perf-baseline.md` for the numbers
that establish it is both reachable and not free.

## Usage

```bash
# Safe current-state report (default output path under specs/016-...):
tools/perf-ab-rig/run-perf-ab.sh

# Launch gate: full A/B against both client binaries, recording the old
# client's numbers into perf-baseline.md as it goes:
tools/perf-ab-rig/run-perf-ab.sh --live \
  --new-client target/release/scribe-client-gpui \
  --old-client target/release/scribe-client \
  --scribe-test target/release/scribe-test --record-baseline
```

Every binary passed to a `--live` run must come from this tree; see
[Live-mode prerequisites](#live-mode-prerequisites) for why the installed
`/usr/bin/scribe-client` is not a usable old-client half.

Workload sizing is tunable: `--samples N` (latency keystrokes),
`--firehose-mib N`, `--tabs N`.

## Verdicts

`PASS` requires all five metrics measured and inside their thresholds. `FAIL` on
any metric fails the gate and reopens the perf bead. `INCOMPLETE` means a metric
could not be captured at all: no display, no server, a missing binary, or a
comparative metric with no committed baseline (`NO-BASELINE`).
