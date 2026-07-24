#!/usr/bin/env bash
#
# Perf A/B rig for the GPUI client rebuild (bead scribe-38e.41).
#
# Measures the five launch-blocking performance metrics from Clarification Q3
# (specs/016-gpui-client-rebuild/spec.md#Clarifications) for the new GPUI
# client and compares them against the recorded old-client baselines, then
# writes a markdown report with a per-metric pass/fail verdict.
#
# The Q3 budget:
#   1. startup-to-first-frame   : <= 500 ms absolute (also gates splash deletion)
#   2. input latency            : no worse than the old client
#   3. cat-firehose throughput  : no worse than the old client
#   4. memory at 10 tabs        : <= old client + 20%
#   5. scroll fps / dropped     : sustained 60 fps with < 1% dropped frames
#
# Two modes:
#   assess (default) -- generate the current-state report from the committed
#     baseline plus a static capability check. Never launches a GUI window, so
#     it is safe to run in CI or a shared session and never touches the live
#     server. Metrics that need a feature-complete client are reported DEFERRED.
#   --live           -- the launch-gate mode. Launches the target client binary
#     on the same machine/session, drives each workload, captures the numbers,
#     and enforces the thresholds. Requires a display and a running server; it
#     attaches to the already-running server and NEVER restarts it.
#
# Usage:
#   tools/perf-ab-rig/run-perf-ab.sh [--live] [--out PATH]
#       [--new-client PATH] [--baseline PATH]
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

MODE="assess"
OUT="${REPO_ROOT}/specs/016-gpui-client-rebuild/perf-ab-report.md"
NEW_CLIENT=""
BASELINE="${REPO_ROOT}/specs/016-gpui-client-rebuild/perf-baseline.md"

# --- Q3 thresholds --------------------------------------------------------
STARTUP_BUDGET_MS=500          # absolute ceiling to first frame
MEM_REGRESSION_MAX_PCT=20      # memory may exceed old client by at most 20%
SCROLL_TARGET_FPS=60           # sustained scroll target
SCROLL_DROPPED_MAX_PCT=1       # dropped-frame ceiling

# --- old-client recorded baseline (from perf-baseline.md) -----------------
# Startup-to-first-frame baseline: init_gpu_and_terminal_done total.
OLD_STARTUP_MS=190

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live) MODE="live"; shift ;;
    --out) OUT="$2"; shift 2 ;;
    --new-client) NEW_CLIENT="$2"; shift 2 ;;
    --baseline) BASELINE="$2"; shift 2 ;;
    -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# --------------------------------------------------------------------------
# Measurement helpers
# --------------------------------------------------------------------------

# Launch the target client with startup instrumentation and parse the
# machine-readable first-frame marker it prints to stderr. Attaches to the
# already-running server; never restarts it. Echoes the milliseconds or "".
measure_startup_first_frame() {
  local bin="$1"
  [[ -x "$bin" ]] || { echo ""; return; }
  local marker
  marker="$(mktemp)"
  rm -f "$marker"
  # The window auto-attaches to the first live pane and paints one frame; a
  # short timeout is enough to reach first paint, then we tear the window down.
  # The client writes `first_frame_ms=<n>` to the marker path on first paint.
  SCRIBE_GPUI_STARTUP_TIMING="$marker" timeout 20s "$bin" >/dev/null 2>&1 || true
  local ms=""
  if [[ -f "$marker" ]]; then
    ms="$(grep -oE 'first_frame_ms=[0-9.]+' "$marker" | head -n1 | cut -d= -f2 || true)"
  fi
  rm -f "$marker"
  echo "$ms"
}

# Compare a measured value against a threshold using awk (float-safe).
# Args: measured op threshold  -> exits 0 (pass) / 1 (fail).
float_cmp() {
  awk -v a="$1" -v b="$3" "BEGIN { exit !(a $2 b) }"
}

# --------------------------------------------------------------------------
# Per-metric evaluation. Each sets METRIC_* globals consumed by the report.
# --------------------------------------------------------------------------

eval_startup() {
  local measured=""
  if [[ "$MODE" == "live" && -n "$NEW_CLIENT" ]]; then
    measured="$(measure_startup_first_frame "$NEW_CLIENT")"
  fi
  if [[ -z "$measured" ]]; then
    STARTUP_STATUS="DEFERRED"
    STARTUP_VALUE="not captured"
    STARTUP_NOTE="Run with --live --new-client <bin> on a machine with a display and a running server. The client is instrumented (SCRIBE_GPUI_STARTUP_TIMING); assess mode never launches a GUI."
    return
  fi
  STARTUP_VALUE="${measured} ms"
  if float_cmp "$measured" "<=" "$STARTUP_BUDGET_MS"; then
    STARTUP_STATUS="PASS"
  else
    STARTUP_STATUS="FAIL"
  fi
  STARTUP_NOTE="Budget ${STARTUP_BUDGET_MS} ms; old-client baseline ${OLD_STARTUP_MS} ms. Splash deletion is authorized only when this PASSes."
}

# The remaining four metrics need a feature-complete client: a stable input
# encoder + echo instrumentation, multi-tab support, and scroll with a frame
# counter. The scaffold spike has none of these, so assess mode marks them
# DEFERRED with the exact live method the launch gate (scribe-38e.42) uses.
eval_runtime_metric() {
  local name="$1" method="$2"
  if [[ "$MODE" == "live" && -n "$NEW_CLIENT" ]]; then
    # Placeholder for launch-gate capture wiring; the spike cannot yet be
    # driven for these workloads, so even --live defers until the client is
    # feature-complete. Kept explicit so the gate operator sees the gap.
    echo "DEFERRED|awaiting feature-complete client|${method}"
  else
    echo "DEFERRED|spike is display-only; not yet measurable|${method}"
  fi
}

# --------------------------------------------------------------------------
# Report generation
# --------------------------------------------------------------------------

emit_report() {
  local now
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  IFS='|' read -r LAT_STATUS LAT_NOTE LAT_METHOD <<<"$(eval_runtime_metric input-latency \
    'Instrumented echo round-trip: send one KeyInput, timestamp the echoed PtyOutput frame, take the median over N samples; compare the new-client median against the old-client median (ratio <= 1.0 within noise).')"
  IFS='|' read -r FIRE_STATUS FIRE_NOTE FIRE_METHOD <<<"$(eval_runtime_metric firehose-throughput \
    'cat a large file inside one pane; measure sustained bytes/sec the client drains without falling behind; compare against the old-client sustained rate (>= within noise).')"
  IFS='|' read -r MEM_STATUS MEM_NOTE MEM_METHOD <<<"$(eval_runtime_metric memory-10-tabs \
    "open 10 tabs with a steady workload, sample steady-state RSS; PASS when new RSS <= old RSS + ${MEM_REGRESSION_MAX_PCT}%.")"
  IFS='|' read -r SCROLL_STATUS SCROLL_NOTE SCROLL_METHOD <<<"$(eval_runtime_metric scroll-fps \
    "drive a sustained scroll over a tall scrollback; read the frame counter; PASS at sustained ${SCROLL_TARGET_FPS} fps with < ${SCROLL_DROPPED_MAX_PCT}% dropped frames.")"

  eval_startup

  local overall="DEFERRED"
  if [[ "$STARTUP_STATUS" == "FAIL" ]]; then overall="FAIL"; fi

  cat >"$OUT" <<EOF
# GPUI Client Perf A/B Report

Generated by \`tools/perf-ab-rig/run-perf-ab.sh\` (mode: \`${MODE}\`) at ${now}.
This is the launch-blocking performance comparison for the GPUI client rebuild
(bead scribe-38e.41), gating cutover in the launch go/no-go (scribe-38e.42).

## Thresholds (Clarification Q3)

| Metric | Threshold |
|---|---|
| Startup to first frame | <= ${STARTUP_BUDGET_MS} ms absolute |
| Input latency | no worse than old client |
| cat-firehose throughput | no worse than old client |
| Memory at 10 tabs | <= old client + ${MEM_REGRESSION_MAX_PCT}% |
| Scroll | sustained ${SCROLL_TARGET_FPS} fps, < ${SCROLL_DROPPED_MAX_PCT}% dropped |

Old-client baselines come from \`${BASELINE#"$REPO_ROOT/"}\`.

## Results

| Metric | New client | Verdict |
|---|---|---|
| Startup to first frame | ${STARTUP_VALUE} | ${STARTUP_STATUS} |
| Input latency | pending | ${LAT_STATUS} |
| cat-firehose throughput | pending | ${FIRE_STATUS} |
| Memory at 10 tabs | pending | ${MEM_STATUS} |
| Scroll fps / dropped frames | pending | ${SCROLL_STATUS} |

**Overall gate verdict: ${overall}.** A DEFERRED overall means the rig and the
old-client baselines are committed but the new client is not yet
feature-complete enough to enforce every threshold; the launch gate re-runs
this rig with \`--live\` once the client is complete. A FAIL reopens the bead.

## Per-metric detail

### Startup to first frame -- ${STARTUP_STATUS}
${STARTUP_NOTE}

### Input latency -- ${LAT_STATUS}
${LAT_NOTE}. Method: ${LAT_METHOD}

### cat-firehose throughput -- ${FIRE_STATUS}
${FIRE_NOTE}. Method: ${FIRE_METHOD}

### Memory at 10 tabs -- ${MEM_STATUS}
${MEM_NOTE}. Method: ${MEM_METHOD}

### Scroll fps / dropped frames -- ${SCROLL_STATUS}
${SCROLL_NOTE}. Method: ${SCROLL_METHOD}

## Reproducing

Assess (safe, no GUI, current-state report):

    tools/perf-ab-rig/run-perf-ab.sh

Launch gate (full A/B on the same machine/session, attaches to the running
server, never restarts it):

    tools/perf-ab-rig/run-perf-ab.sh --live \\
      --new-client target/release/scribe-client-gpui

The client writes the startup marker only when \`SCRIBE_GPUI_STARTUP_TIMING\`
names an output file; the rig sets it to a temp path automatically for the
startup measurement.
EOF

  echo "wrote report: $OUT"
  echo "overall gate verdict: $overall"
}

emit_report
