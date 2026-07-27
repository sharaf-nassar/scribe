# Old Client Performance Baseline

This records the old `scribe-client` baseline that the GPUI rebuild's
performance gate compares on this machine. Startup is recorded **end-to-end**
(process start to GPU-ready/first frame), the same definition the GPUI
client's first-frame marker uses — see the history note below.

## Capture

The capture ran on 2026-07-24 using `/usr/bin/scribe-client`, the installed
old client, connected to the already-running local server (never restarted).
Samples were interleaved with GPUI-client runs in the same session so both
clients saw the same machine load.

| Metric | Baseline | Method |
|---|---:|---|
| Startup to first frame, end-to-end | 3300 ms (median of 7: 3000-5500) | wall clock from the first `client startup timing` log line to `init_gpu_and_terminal_done` |
| GPU-init phase only (NOT the startup baseline) | 635 ms (median of 7: 541-1244) | `init_gpu_and_terminal_done` phase-scoped `total_ms` |
| `load_host_stats` synchronous stall | 1206 ms | `client startup timing` log, single sample |

For comparison, the GPUI client's `first_frame_ms` marker in the same
interleaved runs: 655-1315 ms (median 980 under heavy build load; 674-764 ms,
median 716, on the same host a few minutes earlier under light load).

## History: the 190 ms figure

The 2026-07-24 capture initially recorded "startup to GPU-ready first-frame
path: 190 ms" from `init_gpu_and_terminal_done` `total_ms`. That timer is
phase-scoped — it starts at window/GPU init, AFTER config load, host-stats
sampling (~1.2 s synchronous `sysinfo` walk on this host) and app
construction — so it was never comparable to the GPUI client's
process-start-to-first-frame marker, and even the phase-scoped number is no
longer reproducible on this machine (re-measured 505-1244 ms across
2026-07-24/25; see beads scribe-38e.50 and scribe-38e.83 for the full
tables). The Q3 startup budget was re-scoped accordingly (spec.md,
"Q3 re-scope"): both clients are measured end-to-end and compared A/B on the
same host, and this file's machine-readable slot below now stores the old
client's end-to-end number.

## 2026-07-25 re-capture: first *painted* frame, shared probe

The 2026-07-24 capture above measures the old client's startup from its logs,
which stop at GPU-ready. Bead `scribe-38e.83` re-captured both clients through
the shared runtime probe (`crates/scribe-common/src/perf_probe.rs`), whose
`startup_first_frame_ms` is latched on the first *painted frame* and timed from
the first statement of each client's `main` — identical code on both sides of
the A/B. Both binaries were built from this tree at `target/release/` and
launched cold five times each against the already-running local server, which
was never restarted.

| Client | Samples (ms) | Median |
|---|---|---:|
| Old (`scribe-client`, winit) | 3400.7 / 3401.8 / 3502.8 / 3598.6 / 4681.8 | 3502.8 ms |
| New (`scribe-client-gpui`) | 633.8 / 752.1 / 760.6 / 763.0 / 780.2 | 760.6 ms |

An interleaved second batch under desktop load reproduced the ratio: old
3688 / 3919 / 5465 / 5869 ms against new 721 / 789 / 887 / 1418 ms. A later
`--live --startup-only --record-baseline` rig run scored medians of 3334.4 ms
(old) against 621.4 ms (new), of which 27.1 ms was Scribe's. The old
client's total is dominated by pre-window work its GPU timings never covered —
`load_host_stats` alone is ~1234 ms and `app_constructed` reaches ~2465 ms
before the window is created.

### Startup composition (new client)

From the GPUI client's `SCRIBE_GPUI_STARTUP_TIMING` marker, which times
`cx.open_window` — the span in which no Scribe code runs.

| Span | Samples (ms) | Median |
|---|---|---:|
| `gpu_bringup_ms` (inside `cx.open_window`) | 609.6 / 727.9 / 733.6 / 733.9 / 751.5 | 733.6 ms |
| `scribe_startup_ms` (everything else) | 24.0 / 24.1 / 26.6 / 28.7 / 29.4 | 26.6 ms |

This is the GPU bring-up floor that justifies the Q3 startup re-scope in
`spec.md`, and `scribe_startup_ms` is what the re-scope's absolute 150 ms
budget gates.

### Old-client component timings (log method, retained)

Structured `client startup timing` log lines from the old client. These remain
useful as component references but are **not** first-frame baselines.

| Metric | Value | Method |
|---|---:|---|
| Window + GPU init | 190 ms (2026-07-24), 642 ms (2026-07-25) | `init_gpu_and_terminal_done` total time |
| Config and static-state load | 81 ms (2026-07-24), 1236 ms (2026-07-25) | `startup_state_loaded` total time |
| GPU surface configure | 138 ms (2026-07-24), 464–561 ms (2026-07-25) | `configure_wgpu` total time |
| Terminal renderer construction | 171 ms | `create_terminal_renderer` total time |
| First initial-session creation | 2 ms | `handle_empty_session_list` total time |

## Machine-readable baselines

The A/B rig (`tools/perf-ab-rig/run-perf-ab.sh`) reads the block below and
compares the new client against it. Each value is the old client's number for
that metric on this machine; an empty slot means "not captured yet", and the
rig reports the metric as `NO-BASELINE` rather than passing it. Running the
rig with `--live --old-client <bin> --record-baseline` re-measures the old
client with the same probe in the same session and writes the values back
here. `startup_first_frame_ms` is end-to-end (see above), median of the rig's
startup samples. The rig prefers the probe whenever the launched binary reports
it and falls back to the log otherwise.

The first four slots were captured on 2026-07-27 by the `scribe-38e.42`
launch-gate run, from an old client **rebuilt from this tree**
(`target/release/scribe-client`).
That detail is load-bearing: the *installed* `/usr/bin/scribe-client` predates
the shared probe, so with it the rig's `start_client` never sees a probe report
and every workload phase gives up with "client … never reached a first frame",
which surfaces as three `NO-BASELINE` verdicts rather than as the missing
prerequisite it actually is. The same run also needs `--scribe-test` pointed at
a real binary, or the rig logs "cannot seed a session for the client" and
continues without one. The command that produced these values:

    tools/perf-ab-rig/run-perf-ab.sh --live \
      --new-client target/release/scribe-client-gpui \
      --old-client target/release/scribe-client \
      --scribe-test target/release/scribe-test \
      --record-baseline

    perf_baseline_startup_first_frame_ms=4571.975
    perf_baseline_input_latency_p50_ms=0.247
    perf_baseline_firehose_bytes_per_sec=11154443.570
    perf_baseline_memory_rss_kb=476916
    perf_baseline_scroll_fps=41.480
    perf_baseline_scroll_dropped_pct=8.101

The `input_latency_p50_ms` and `firehose_bytes_per_sec` slots were re-recorded
later the same day by bead `scribe-38e.92`, from a full `--live
--record-baseline` run against the isolated `scribe-dev` server. Both are
supersessions rather than re-measurements: the values they replaced (0.032 and
243217.780) were produced by instrumentation that stamped the old client's PTY
output on its UI thread, and nothing the current probe reports is comparable to
them. See the section below.

The two `scroll_*` slots were added on 2026-07-27 by bead `scribe-38e.91` and
recorded by a `--live --scroll-only --record-baseline` run against the isolated
`scribe-dev` server. Scroll is an absolute threshold, so these are not a
comparison input: they are the attribution that tells a client regression apart
from an unreachable target, which is what the metric lacked. See the section
below.

## 2026-07-27: input latency was measuring the UI-thread backlog

Metric 2 read 0.209 ms for the new client against 0.032 ms for the old one — a
6.5x regression against a 10% allowance, and the reason bead `scribe-38e.92`
was opened. The client was not at fault; the A/B was comparing two different
quantities.

The two clients stamped PTY output at different pipeline stages. The GPUI
client counts it in its IPC read task, the moment the frame comes off the
socket. The old client counted it on its UI thread in
`handle_stream_user_event`, three hops downstream: read task,
`EventLoopProxy::send_event`, winit's unbounded user-event queue, redraw loop.
Behind that queue the probe reports the UI thread's backlog rather than the
server round trip, and it corrupted both probe-derived comparative metrics in
opposite directions.

**Input latency.** With a backlog standing in the queue, the first
`handle_stream_user_event` after a keystroke was an already-queued stale
payload, so the sample timed one event-loop turn instead of a key-to-echo trip.
The tell is that 0.032 ms is *faster than a bare local socketpair round trip
measured on this host* — 0.07-0.12 ms p50 over 25 spaced samples through the
same "unbounded channel to a writer task, reader task closes the clock" shape
both clients use. No keystroke can reach the server, echo through a PTY and
come back in less time than one socket hop.

**Throughput.** The same backlog stretched the firehose window: 32 MiB of `cat`
output reaches the socket far sooner than it reaches the UI thread, which is
how the old client scored 0.232 MiB/s.

Both clients now stamp PTY output where it enters the process, and the echo
pairing is scoped to the session the keystroke was routed to, so a background
pane's output cannot close another pane's clock. An unmatched keystroke
releases the pairing slot after one second instead of holding it for the rest of
the run. The sample count also moved from 25 to 60: in the 0.2-0.4 ms band the
median of 25 moved by more than the 10% allowance between back-to-back runs of
the *same* binary (0.260 then 0.366 ms), enough to decide the verdict by itself.

Re-measured that way, on the reference host against the `scribe-dev` server,
release builds from this tree:

| Metric | New (`scribe-client-gpui`) | Old (`scribe-client`) | Verdict |
|---|---:|---:|---|
| Input latency p50 | 0.213 ms | 0.247 ms | PASS |
| cat-firehose | 17.922 MiB/s | 10.638 MiB/s | PASS |

Two earlier 60-sample latency-only runs agree on the direction: 0.235 against
0.330 ms, and 0.216 against 0.359 ms. The reported regression was the
measurement; the new client is in fact slightly faster on both metrics. Both
absolute latencies are sub-millisecond either way, and the probe measures an
in-process span rather than user-perceived latency, so neither number was ever
going to be visible to a user — but the criterion is comparative, and it now
compares like with like.

## 2026-07-27: the scroll metric was measuring the rig

Metric 5 reproducibly read ~29 fps with 13-16% dropped frames against a target
of sustained 60 fps with `< 1%` dropped, and the rig had no old-client arm to
attribute it with. Bead `scribe-38e.91` added the arm and measured both clients
under the identical workload. Neither the target nor the client turned out to be
the problem: two independent defects in the rig were, and both understated the
new client specifically.

**The measurement window was inflated by the client's own idle.** The window is
derived from the shared probe's `uptime_ms` stamps, and the probe rewrites its
report only when the client paints or drains PTY bytes. After the rig's
four-second wait for `less` to settle, a client that had gone quiet still
carried the stamp from whenever it last drew, so the frame counts were right
while the elapsed time was four seconds too long — an fps understated by roughly
45%. It understated the client that idles *best* the most: traced at 50 ms
resolution, the GPUI client's report was 4.1 s stale at the start of the drive
because it stops painting entirely, against 0.5 s for the old client, which kept
draining bytes. The A/B read that difference as a paint-path regression. The rig
now waits for a report rewrite before opening the window.

**The workload could not demand 60 fps.** It paged `less` with synthetic `space`
events, and each page-forward produces exactly one repaint — so the frame rate
was pinned to the rate at which `xdotool` could deliver synthetic keys. Measured
directly, `xdotool key --repeat 60 --repeat-delay 8` takes 1262-1284 ms per
batch, i.e. **21 ms per key**, not the requested 8 ms: a hard ceiling near
47 fps. Both clients sat exactly on it, painting one frame per delivered key at
a steady 21 ms interval and 5-10% of one CPU core — the signature of a workload
bottleneck rather than a renderer one.

Driving the scroll from an unpaced writer inside the pane (`seq 1 100000000`,
no keys sent while the window is open) removes the rig from the loop. Measured
that way on the reference host, release builds from this tree, against the
`scribe-dev` server:

| Run | New (`scribe-client-gpui`) | Old (`scribe-client`) |
|---|---|---|
| 1 | 59.942 fps, 0.000% dropped | 50.575 fps, 5.841% dropped |
| 2 | 59.597 fps, 0.416% dropped | 48.770 fps, 6.601% dropped |
| 3 | 60.064 fps, 0.000% dropped | 41.480 fps, 8.101% dropped |

The GPUI client sustains the display's full 60 Hz with no dropped frames; the
old client reaches 41-51 fps with 6-8% dropped and does not meet the target on
the same workload. The absolute Clarification-Q3 threshold therefore stands
unchanged — it is both reachable and not free — and the metric now discriminates
between the two clients instead of reporting the rig's synthetic-input rate for
both. No client code changed: a candidate fix to the GPUI redraw pump's sampling
interval was measured at 59.9 fps against 60.0 fps for the unmodified client and
dropped as unjustified.

## Remaining gate measurements

Input echo latency, `cat` firehose throughput, and memory at ten tabs are
captured live by the rig, not inferred from a sandboxed launch: both clients
carry the same probe (`crates/scribe-common/src/perf_probe.rs`), so the A/B
compares identical measurement points under an identical workload — identical
down to the pipeline stage, which is what the input-latency section above had
to fix. All those
slots are now filled (see above), so the gate scores them rather than reporting
`NO-BASELINE`.
