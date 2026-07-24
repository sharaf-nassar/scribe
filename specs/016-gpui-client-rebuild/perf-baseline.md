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

## Remaining gate measurements

Input echo latency, `cat` firehose throughput, memory at ten tabs, and scroll
FPS require an instrumented session and controlled ten-tab workload. They are
intentionally not inferred from a sandboxed launch: the A/B rig in
`scribe-38e.41` must capture those values with the same workload before it
enforces a regression threshold. This file is the committed old-client startup
baseline and records the exact method so that rig can reproduce it.
