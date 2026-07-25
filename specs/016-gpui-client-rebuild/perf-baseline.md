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
startup samples. The recorded 3334.415 ms is the probe method (first painted
frame) against an old client rebuilt from this tree; the log method against the
installed binary scored 3697 ms on the same host. The rig prefers the probe
whenever the launched binary reports it and falls back to the log otherwise.

    perf_baseline_startup_first_frame_ms=3334.415
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
