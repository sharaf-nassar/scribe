# Old Client Performance Baseline

This records the old `scribe-client` launch baseline that the GPUI rebuild's
performance gate compares on this machine.

## Capture

The capture ran on 2026-07-24 using `/usr/bin/scribe-client`, the installed
old client. The client connected to the already-running local server; it was
not restarted. Values come from the client's structured startup timing logs.

| Metric | Baseline | Method |
|---|---:|---|
| Startup to GPU-ready first-frame path | 190 ms | `init_gpu_and_terminal_done` total time |
| Config and static-state load | 81 ms | `startup_state_loaded` total time |
| GPU surface configure | 138 ms | `configure_wgpu` total time |
| Terminal renderer construction | 171 ms | `create_terminal_renderer` total time |
| First initial-session creation | 2 ms | `handle_empty_session_list` total time |

## Machine-readable baselines

The A/B rig (`tools/perf-ab-rig/run-perf-ab.sh`) reads the block below and
compares the new client against it. Each value is the old client's number for
that metric on this machine; an empty slot means "not captured yet", and the
rig reports the metric as `NO-BASELINE` rather than passing it. Running the rig
with `--live --old-client <bin> --record-baseline` re-measures the old client
with the same probe in the same session and writes the values back here.

    perf_baseline_startup_first_frame_ms=190
    perf_baseline_input_latency_p50_ms=
    perf_baseline_firehose_bytes_per_sec=
    perf_baseline_memory_rss_kb=

## Remaining gate measurements

Input echo latency, `cat` firehose throughput, and memory at ten tabs are
captured live by the rig, not inferred from a sandboxed launch: both clients
carry the same probe (`crates/scribe-common/src/perf_probe.rs`), so the A/B
compares identical measurement points under an identical workload. Until the
slots above are filled by a `--record-baseline` run, those three metrics have no
comparison point and the gate stays `INCOMPLETE`. Scroll fps is an absolute
target (sustained 60 fps, `< 1%` dropped) and needs no old-client baseline.
