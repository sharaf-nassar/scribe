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

## Machine-readable baselines

The A/B rig (`tools/perf-ab-rig/run-perf-ab.sh`) reads the block below and
compares the new client against it. Each value is the old client's number for
that metric on this machine; an empty slot means "not captured yet", and the
rig reports the metric as `NO-BASELINE` rather than passing it. Running the
rig with `--live --old-client <bin> --record-baseline` re-measures the old
client with the same probe in the same session and writes the values back
here. `startup_first_frame_ms` is end-to-end (see above), median of the rig's
startup samples.

    perf_baseline_startup_first_frame_ms=3697
    perf_baseline_input_latency_p50_ms=
    perf_baseline_firehose_bytes_per_sec=
    perf_baseline_memory_rss_kb=

## Remaining gate measurements

Input echo latency, `cat` firehose throughput, and memory at ten tabs are
captured live by the rig, not inferred from a sandboxed launch: both clients
carry the same probe (`crates/scribe-common/src/perf_probe.rs`), so the A/B
compares identical measurement points under an identical workload. Until the
slots above are filled by a `--record-baseline` run, those three metrics have
no comparison point and the gate stays `INCOMPLETE`. Scroll fps is an absolute
target (sustained 60 fps, `< 1%` dropped) and needs no old-client baseline.
