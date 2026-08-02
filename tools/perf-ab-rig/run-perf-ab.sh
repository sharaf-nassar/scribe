#!/usr/bin/env bash
#
# Perf A/B rig for the GPUI client rebuild (beads scribe-38e.41 / scribe-38e.51).
#
# Measures the five launch-blocking performance metrics from Clarification Q3
# (specs/016-gpui-client-rebuild/spec.md#Clarifications) for the new GPUI
# client, compares them against the old client measured with the SAME
# instrumentation, and writes a markdown report with a per-metric pass/fail
# verdict.
#
# The Q3 budget (startup re-scoped 2026-07-24, amended 2026-07-25 by bead
# scribe-38e.83; see spec.md "Q3 re-scope"):
#   1a. Scribe-attributable startup : <= 150 ms absolute (everything outside
#                                     gpui's window/GPU bring-up)
#   1b. startup-to-first-frame      : no worse than the old client, both
#                                     measured end-to-end on the same
#                                     host/session
#   2.  input latency               : no worse than the old client
#   3.  cat-firehose throughput     : no worse than the old client
#   4.  memory at 10 tabs           : <= old client + 20%
#   5.  scroll fps / dropped        : sustained 60 fps with < 1% dropped frames
#
# Every metric is read from the shared runtime probe both clients link
# (crates/scribe-common/src/perf_probe.rs), armed by SCRIBE_PERF_PROBE, so both
# halves of the A/B are measured by identical code. The GPUI client additionally
# writes SCRIBE_GPUI_STARTUP_TIMING, which splits its first-frame time into the
# gpui GPU bring-up floor and the Scribe-attributable remainder that metric 1a
# gates. Memory is sampled externally from /proc so it is client-agnostic.
#
# Two modes:
#   assess (default) -- generate the current-state report from the committed
#     baseline plus a static capability check. Never launches a GUI window, so
#     it is safe in CI or a shared session and never touches the live server.
#     Metrics that need a live client are reported NOT-MEASURED.
#   --live           -- the launch-gate mode. Launches the target client
#     binaries on the same machine/session, drives each workload through
#     xdotool, captures the numbers and enforces the thresholds. It attaches to
#     the already-running server and NEVER restarts it.
#
# Live-mode safety: every workload types into a tab the rig itself opened and
# watched appear in the probe's session list. If the new tab does not show up,
# or focus is not on it, the workload aborts rather than typing into a pane that
# was already open. The rig closes the tabs it opened when it is done.
#
# Live mode drives the isolated `scribe-dev` install, never the stable one. See
# `stage_dev_binary`: every binary the rig launches is copied to a staging path
# named `scribe-dev`, which is what `AppIdentity::detect_from_path` keys off, so
# the sockets, config and state the run touches all live under the `scribe-dev`
# slug. Without that a gate run seeds sessions, opens ten tabs and types `exit`
# into the developer's own live terminal.
#
# Usage:
#   tools/perf-ab-rig/run-perf-ab.sh [--live] [--startup-only] [--scroll-only]
#       [--latency-only] [--ai-tab-only]
#       [--out PATH] [--new-client PATH] [--old-client PATH] [--baseline PATH]
#       [--record-baseline] [--samples N] [--firehose-mib N] [--tabs N]
#       [--scribe-test PATH]
#
# --startup-only limits a --live run to metric 1 (no tabs are opened and no
# keys are typed); metrics 2-5 report NOT-MEASURED. This is the fast loop for
# the startup perf bead. --scroll-only is the same idea for metric 5, which is
# the one metric measured on both clients purely to attribute a failure, and
# --latency-only for metric 2. --ai-tab-only is a separate Q6 gate: it times
# the Claude-tab chord to the first PTY bytes from a marker stub named `claude`
# on PATH, reports its result directly, and does not write the five-metric A/B
# report.
#
# A full --live run has two hard prerequisites, both checked up front and both
# fatal rather than degraded (see `live_preflight`): the `scribe-test` helper
# that seeds the session the client attaches to, and client binaries that
# actually carry the shared probe. Point --scribe-test and the client flags at
# binaries built from this tree; an installed client from an older release
# predates the probe and cannot be measured.
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

MODE="assess"
# Empty means "score all five metrics"; otherwise the single metric a --live run
# is limited to. `metric_enabled` is the only reader.
ONLY_METRIC=""
AI_TAB_ONLY=0
OUT="${REPO_ROOT}/specs/016-gpui-client-rebuild/perf-ab-report.md"
NEW_CLIENT=""
OLD_CLIENT=""
BASELINE="${REPO_ROOT}/specs/016-gpui-client-rebuild/perf-baseline.md"
RECORD_BASELINE=0
# Helper used to seed (and later close) the detached session the client attaches
# to. Same binary the visual E2E entrypoint drives.
SCRIBE_TEST_BIN="scribe-test"
# Env var that arms the shared runtime probe. Its presence as a literal string
# inside a client binary is what tells the rig that binary can be measured at
# all; see `client_has_probe`.
PROBE_ENV_KEY="SCRIBE_PERF_PROBE"
# File stem that selects the isolated dev install. `AppIdentity::detect_from_path`
# (crates/scribe-common/src/app.rs) derives the whole runtime identity — socket
# directory, config directory, state directory — from the running executable's
# stem, so staging a binary under this name is what keeps a live gate run off the
# developer's stable server. There is no env override to use instead.
DEV_EXE_STEM="scribe-dev"

# --- Q3 thresholds --------------------------------------------------------
# Startup is a same-host A/B against the old client (re-scoped from the
# original absolute 500 ms on 2026-07-24: the recorded 190 ms old-client
# "baseline" was a phase-scoped GPU-init timer, not process-start-to-first-
# frame, and the GPU bring-up floor alone exceeds 500 ms on the reference
# host; see spec.md "Q3 re-scope" and perf-baseline.md). Both clients are
# measured end-to-end and compared with the shared noise allowance.
#
# The 2026-07-25 amendment (bead scribe-38e.83) adds the absolute half: gpui
# spends 610-1315 ms inside `cx.open_window` (wgpu adapter enumeration, device
# creation, surface configure) before any Scribe code runs, so the total is
# dominated by a platform floor this repo cannot move. What it can move is the
# remainder, and that is capped absolutely.
STARTUP_SAMPLES=3              # startup runs per client; the median is scored
SCRIBE_STARTUP_BUDGET_MS=150   # absolute ceiling on Scribe-attributable startup
MEM_REGRESSION_MAX_PCT=20      # memory may exceed old client by at most 20%
SCROLL_TARGET_FPS=60           # sustained scroll target
SCROLL_DROPPED_MAX_PCT=1       # dropped-frame ceiling
AI_TAB_BUDGET_MS=1000          # Q6 soft budget: approximately one second
AI_TAB_STUB_MARKER="SCRIBE_AI_TAB_PERF_MARKER"
# "No worse than the old client" is enforced with this run-to-run noise
# allowance on both sides of the comparison; it is not extra headroom, it is the
# measurement's own repeatability on a loaded desktop.
NOISE_TOLERANCE_PCT=10

# --- workload sizing ------------------------------------------------------
# Keystrokes echoed per latency run. Both clients land in the 0.2-0.4 ms band,
# where desktop jitter is a large fraction of the measurement: at the original
# 25 the median moved by more than the 10% allowance between back-to-back runs
# of the same binary (0.260 then 0.366 ms), enough to decide the gate by
# itself. At 60 the medians repeat inside the allowance.
LATENCY_SAMPLES=60
FIREHOSE_MIB=32                # size of the file `cat`ted for throughput
MEMORY_TABS=10                 # tab count for the memory metric
SCROLL_SECONDS=8               # sustained scroll drive time
SCROLL_SETTLE_SECONDS=2        # time the scroll reaches steady state before it is measured
# The scroll workload scrolls the live grid from a command running INSIDE the
# pane, and sends no keys at all while it is measured.
#
# It used to page `less` with synthetic `space` events, and that made the metric
# measure the rig instead of the client (bead scribe-38e.91). Every page-forward
# produces exactly one repaint, so the frame rate was pinned to the rate at which
# `xdotool` could deliver synthetic keys — measured at 21 ms per key on the
# reference host, i.e. a hard ceiling of ~47 fps, against a 60 fps target.
# BOTH clients sat on that ceiling, which is what made a workload artefact look
# like a paint-path regression. An unpaced writer instead asks the client for
# every frame the display can show, which is what "sustained 60 fps" means.
SCROLL_COMMAND="seq 1 100000000"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live) MODE="live"; shift ;;
    --startup-only) ONLY_METRIC="startup"; shift ;;
    --scroll-only) ONLY_METRIC="scroll"; shift ;;
    --latency-only) ONLY_METRIC="latency"; shift ;;
    --ai-tab-only) AI_TAB_ONLY=1; shift ;;
    --out) OUT="$2"; shift 2 ;;
    --new-client) NEW_CLIENT="$2"; shift 2 ;;
    --old-client) OLD_CLIENT="$2"; shift 2 ;;
    --baseline) BASELINE="$2"; shift 2 ;;
    --record-baseline) RECORD_BASELINE=1; shift ;;
    --samples) LATENCY_SAMPLES="$2"; shift 2 ;;
    --firehose-mib) FIREHOSE_MIB="$2"; shift 2 ;;
    --tabs) MEMORY_TABS="$2"; shift 2 ;;
    --scribe-test) SCRIBE_TEST_BIN="$2"; shift 2 ;;
    -h|--help) sed -n '2,73p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ "$AI_TAB_ONLY" -eq 1 && -n "$ONLY_METRIC" ]]; then
  echo "--ai-tab-only cannot be combined with another --*-only mode" >&2
  exit 2
fi

# How synthetic keys are delivered. `window` addresses the client window
# directly, so a keystroke cannot escape into another application even if focus
# moves mid-workload; `xtest` is the fallback for a toolkit that reads keys
# through XInput2 and therefore ignores window-targeted synthetic events, and it
# requires the client window to hold X input focus. `open_owned_tab` picks
# whichever mode actually opens a tab and the choice sticks for the run.
KEY_MODE="window"

WORK_DIR="$(mktemp -d)"
CLIENT_PID=""
PROBE_FILE=""
OWNED_SESSION=""
CLEANUP_SESSIONS=""
MEASURED_TABS=""
TAB_OPEN_STARTED_NS=""
TAB_FIRST_PTY_NS=""
AI_TAB_ELAPSED_MS=""

# --------------------------------------------------------------------------
# Small helpers
# --------------------------------------------------------------------------

log() { echo "[perf-ab] $*" >&2; }

# Whether the named metric is scored on this run. Empty ONLY_METRIC scores all
# five; --startup-only, --latency-only and --scroll-only narrow it to one.
metric_enabled() {
  [[ -z "$ONLY_METRIC" || "$ONLY_METRIC" == "$1" ]]
}

# Abort the run with a diagnosis. Used only for conditions under which a --live
# gate run cannot produce valid numbers at all, so that it fails loudly instead
# of emitting a report full of NO-BASELINE rows that read as "nobody has
# captured baselines yet" (bead scribe-38e.97).
# The first argument is the headline; any further arguments are logged as
# indented continuation lines so the remedy stays next to the diagnosis.
die() {
  log "fatal: $1"
  shift
  local line
  for line in "$@"; do
    log "       $line"
  done
  exit 3
}

# Compare two numbers with awk (float-safe). Args: a op b -> exit 0 when true.
float_cmp() {
  awk -v a="$1" -v b="$3" "BEGIN { exit !(a $2 b) }"
}

# Evaluate an awk expression over named variables and print the result.
# Args: expr [name=value ...]
calc() {
  local expr="$1"; shift
  local assignments=()
  local pair
  for pair in "$@"; do
    assignments+=(-v "$pair")
  done
  awk "${assignments[@]}" "BEGIN { printf \"%.3f\n\", $expr }"
}

# Read one `key=value` line out of a probe report (last occurrence wins).
probe_value() {
  local file="$1" key="$2"
  [[ -f "$file" ]] || { echo ""; return; }
  awk -F= -v key="$key" '$1 == key { value = $2 } END { print value }' "$file"
}

# Read a committed baseline number: `perf_baseline_<key>=<value>` in the
# baseline markdown. Empty when the baseline has not been captured yet.
baseline_value() {
  local key="$1"
  [[ -f "$BASELINE" ]] || { echo ""; return; }
  awk -F= -v key="perf_baseline_${key}" \
    '{ gsub(/[[:space:]]/, "", $1) } $1 == key { gsub(/[[:space:]]/, "", $2); value = $2 } END { print value }' \
    "$BASELINE"
}

# Persist a measured old-client number into the committed baseline file.
record_baseline() {
  local key="$1" value="$2"
  # Metrics that carry a diagnostic suffix (`<value>#<tabs>`) record the value.
  value="${value%%#*}"
  # A workload that produced only its diagnostic half (`#2` from a memory run
  # that never reached its tab count) must not blank a good committed baseline:
  # the next run would then read NO-BASELINE and the gate would go INCOMPLETE
  # because of one bad run rather than because nothing was ever captured.
  [[ -n "$value" ]] || { log "no ${key} measured; leaving the committed baseline alone"; return 0; }
  [[ "$RECORD_BASELINE" -eq 1 ]] || return 0
  [[ -f "$BASELINE" ]] || return 0
  local prefix="    perf_baseline_${key}="
  if grep -q "^${prefix}" "$BASELINE"; then
    sed -i "s|^${prefix}.*|${prefix}${value}|" "$BASELINE"
    log "recorded baseline ${key}=${value}"
  else
    log "baseline file has no ${key} slot; not recorded"
  fi
}

# --------------------------------------------------------------------------
# Dev-flavor isolation
# --------------------------------------------------------------------------
#
# A live run creates sessions, opens ten tabs, types shell commands into them
# and then types `exit` to close them again. Pointed at the stable install that
# all lands in whatever terminal the developer is actually using, so the rig
# never runs against it.
#
# Scribe derives its whole runtime identity from the running executable's file
# stem (`AppIdentity::detect_from_path`): a binary called `scribe-dev` uses
# `/run/user/<uid>/scribe-dev/server.sock`, `~/.config/scribe-dev` and
# `~/.local/state/scribe-dev`, while anything else uses the stable slug. There
# is no environment override, so the only way to retarget a binary is to run it
# from a path with that stem — which is exactly what this does. The copy is
# byte-identical to what was passed in, so the numbers still describe the
# binary under test.

# Copy one binary into the run's staging area under the dev stem and echo the
# staged path. `role` only keeps the copies in separate directories, since they
# all end up with the same file name.
stage_dev_binary() {
  local role="$1" src="$2"
  local dir="$WORK_DIR/dev/$role"
  local staged="$dir/$DEV_EXE_STEM"
  mkdir -p "$dir"
  cp -f "$src" "$staged" || return 1
  chmod +x "$staged"
  echo "$staged"
}

# Restage every binary a live run launches, rewriting the globals in place so no
# call site can accidentally reach for the original path.
stage_live_binaries() {
  local resolved
  if [[ -n "$NEW_CLIENT" ]]; then
    NEW_CLIENT="$(stage_dev_binary new-client "$NEW_CLIENT")"
  fi
  if [[ -n "$OLD_CLIENT" ]]; then
    OLD_CLIENT="$(stage_dev_binary old-client "$OLD_CLIENT")"
  fi
  if resolved="$(command -v "$SCRIBE_TEST_BIN" 2>/dev/null)"; then
    SCRIBE_TEST_BIN="$(stage_dev_binary scribe-test "$resolved")"
  fi
  log "staged under the ${DEV_EXE_STEM} runtime slug: $WORK_DIR/dev"
}

# --------------------------------------------------------------------------
# Live-mode plumbing: launch, drive and tear down a client
# --------------------------------------------------------------------------

stop_client() {
  [[ -n "$CLIENT_PID" ]] || return 0
  kill "$CLIENT_PID" 2>/dev/null || true
  wait "$CLIENT_PID" 2>/dev/null || true
  CLIENT_PID=""
}

# Kill anything a measurement subshell launched but could not reap. Each launch
# appends its pid to a file because a measurement runs inside a command
# substitution, and that subshell's variables never reach this trap.
cleanup() {
  stop_client
  local pid
  if [[ -f "$WORK_DIR/pids" ]]; then
    while read -r pid; do
      [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done <"$WORK_DIR/pids"
  fi
  close_seeded_sessions
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT
# Bash only runs the EXIT trap for a signal it has a handler for, so without
# these an interrupted or `timeout`-killed run leaves its client processes and,
# worse, the sessions it seeded and the tabs it opened behind on the server it
# was driving. Re-raising keeps the exit status honest.
trap 'cleanup; trap - INT; kill -INT $$' INT
trap 'cleanup; trap - TERM; kill -TERM $$' TERM

# Seed one detached session so a launched client has a workspace to attach to.
#
# A client that claims an empty window has no workspace, and both clients then
# refuse to open a tab ("no workspace is attached yet"), which would leave every
# workload metric unmeasurable. The rig therefore creates a session through the
# same `scribe-test` helper the visual E2E entrypoint uses and stops the helper
# daemon again, leaving the session detached in a free window for the client to
# adopt. The session is closed again in cleanup.
seed_session() {
  command -v "$SCRIBE_TEST_BIN" >/dev/null 2>&1 || {
    log "no ${SCRIBE_TEST_BIN} on PATH; cannot seed a session for the client"
    return 1
  }
  "$SCRIBE_TEST_BIN" daemon start >/dev/null 2>&1 || return 1
  local session
  session="$("$SCRIBE_TEST_BIN" session create 2>/dev/null | tr -d '[:space:]')"
  "$SCRIBE_TEST_BIN" daemon stop >/dev/null 2>&1 || true
  [[ -n "$session" ]] || { log "seeding a session failed"; return 1; }
  echo "$session" >>"$WORK_DIR/seeded"
  log "seeded detached session ${session} for the client to attach to"
}

close_seeded_sessions() {
  [[ -f "$WORK_DIR/seeded" ]] || return 0
  command -v "$SCRIBE_TEST_BIN" >/dev/null 2>&1 || return 0
  "$SCRIBE_TEST_BIN" daemon start >/dev/null 2>&1 || return 0
  local session
  while read -r session; do
    [[ -n "$session" ]] && "$SCRIBE_TEST_BIN" session close "$session" >/dev/null 2>&1
  done <"$WORK_DIR/seeded"
  "$SCRIBE_TEST_BIN" daemon stop >/dev/null 2>&1 || true
  rm -f "$WORK_DIR/seeded"
}

# Prerequisites for any live workload. Echoes a reason when live mode cannot
# run, empty when it can.
#
# The server it looks for is the isolated dev one, because that is the only
# server a live run is allowed to drive; see the dev-flavor isolation block.
live_blocker() {
  [[ -n "${DISPLAY:-}" ]] || { echo "no DISPLAY is set, so no client can be driven."; return; }
  command -v xdotool >/dev/null 2>&1 || { echo "xdotool is not installed, so no workload can be driven."; return; }
  local socket
  socket="/run/user/$(id -u)/${DEV_EXE_STEM}/server.sock"
  [[ -S "$socket" ]] || { echo "no running ${DEV_EXE_STEM} server at ${socket} to attach to; install and start one with \`just install-dev\` (the rig never drives the stable server)."; return; }
  echo ""
}

# True when a client binary carries the shared runtime probe.
#
# The probe is armed through an environment variable, so its name is a literal
# string in any binary that links `crates/scribe-common/src/perf_probe.rs`. A
# client built before that landed — notably an installed `/usr/bin/scribe-client`
# from an older release — simply never writes a probe report, and every rig wait
# that keys off the report file then burns its full timeout.
client_has_probe() {
  local bin="$1"
  [[ -r "$bin" ]] || return 1
  grep -qa -- "$PROBE_ENV_KEY" "$bin" 2>/dev/null
}

# Fatal preflight for a --live run, checked before any client is launched.
#
# Both conditions here used to degrade silently: a missing `scribe-test` logged
# "continuing without a seeded session" and left the client with no workspace to
# open tabs in, and a probe-less client binary was only noticed 30 s later as
# "never reached a first frame". Either way the three workload metrics came out
# NO-BASELINE, which reads as "no baselines have been captured yet" rather than
# "the run was handed inputs it cannot measure" — the misdiagnosis that cost two
# full gate runs in scribe-38e.42. A launch gate that cannot produce valid
# numbers now says so and stops.
live_preflight() {
  local bin label
  for label in new old; do
    [[ "$AI_TAB_ONLY" -eq 1 && "$label" == "old" ]] && continue
    if [[ "$label" == new ]]; then bin="$NEW_CLIENT"; else bin="$OLD_CLIENT"; fi
    [[ -n "$bin" ]] || continue
    [[ -x "$bin" ]] || die \
      "--${label}-client ${bin} is not an executable file." \
      "Build it from this tree, e.g. \`cargo build --release\`, and pass the" \
      "path under target/release."
    client_has_probe "$bin" && continue
    # Metric 1 has a documented fallback to the startup-log method for a
    # probe-less binary, so a --startup-only run stays valid; metrics 2-5 have
    # no fallback and would time out instead.
    if [[ "$ONLY_METRIC" == "startup" ]]; then
      log "--${label}-client ${bin} carries no ${PROBE_ENV_KEY}; startup will fall back to its startup log"
      continue
    fi
    die \
      "--${label}-client ${bin} was built without the shared perf probe: the" \
      "binary contains no ${PROBE_ENV_KEY} string, so it can never write a probe" \
      "report and the tab-driven workloads would each" \
      "time out as \"never reached a first frame\"." \
      "Pass a client built from this tree, e.g." \
      "--${label}-client target/release/scribe-client, or re-run with" \
      "--startup-only, whose startup-log fallback does not need the probe."
  done

  # Startup-only runs open no tabs, so they need no seeded session.
  if [[ "$ONLY_METRIC" == "startup" ]]; then
    stage_live_binaries
    return 0
  fi

  command -v "$SCRIBE_TEST_BIN" >/dev/null 2>&1 || die \
    "no usable ${SCRIBE_TEST_BIN}: the rig needs it to seed the detached session" \
    "the client attaches to, and without a workspace both clients refuse to open" \
    "a tab, leaving every workload metric unmeasurable." \
    "Pass --scribe-test target/release/scribe-test (build it with" \
    "\`cargo build --release -p scribe-test\`), or re-run with --startup-only," \
    "which opens no tabs."
  # Everything the run launches from here on is the staged dev-slug copy, so the
  # seeded session and every tab land in the isolated dev server.
  stage_live_binaries
  seed_session || die \
    "${SCRIBE_TEST_BIN} could not seed a session for the client to attach to." \
    "Check that the server socket is healthy and re-run; --startup-only skips" \
    "this step entirely."
}

# Resolve and validate the marker stub before an AI-tab measurement. Merely
# finding a `claude` command is not enough: accidentally timing the real CLI
# would include its startup and make this Q6 measurement meaningless.
ai_tab_stub() {
  local stub
  stub="$(command -v claude 2>/dev/null || true)"
  [[ -n "$stub" && -x "$stub" && -r "$stub" ]] || return 1
  grep -qa -- "$AI_TAB_STUB_MARKER" "$stub" 2>/dev/null || return 1
  echo "$stub"
}

# The X11 window id owned by $CLIENT_PID, or empty.
client_window() {
  [[ -n "$CLIENT_PID" ]] || { echo ""; return; }
  xdotool search --onlyvisible --pid "$CLIENT_PID" 2>/dev/null | head -n1
}

# Launch $1 under the probe and wait for its window and first report.
#
# Sets CLIENT_PID and PROBE_FILE rather than echoing them: a command
# substitution would run this in a subshell, and the pid needed to drive and
# later kill the client would be lost with it. Returns non-zero on failure.
start_client() {
  local bin="$1"
  PROBE_FILE="$WORK_DIR/probe.txt"
  [[ -x "$bin" ]] || { log "not executable: $bin"; return 1; }
  rm -f "$PROBE_FILE"
  SCRIBE_PERF_PROBE="$PROBE_FILE" SCRIBE_DISABLE_ANIMATIONS=1 \
    "$bin" >"$WORK_DIR/client.log" 2>&1 &
  CLIENT_PID=$!
  echo "$CLIENT_PID" >>"$WORK_DIR/pids"
  local waited=0
  while [[ $waited -lt 300 ]]; do
    if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
      log "client $bin exited during startup; see $WORK_DIR/client.log"
      CLIENT_PID=""
      return 1
    fi
    if [[ -s "$PROBE_FILE" ]] && [[ -n "$(client_window)" ]]; then
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  log "client $bin never reached a first frame"
  return 1
}

# Wait until the client reports at least one attached session.
#
# Both clients refuse to open a tab before a workspace is attached, so a
# workload that starts too early (or against an empty window) would silently
# measure nothing. Returns non-zero when the client stays empty.
wait_for_attached_session() {
  local waited=0 sessions
  while [[ $waited -lt 100 ]]; do
    sessions="$(probe_value "$PROBE_FILE" sessions)"
    [[ "${sessions:-0}" -ge 1 ]] && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  log "client attached to an empty window; no workspace to open tabs in"
  return 1
}

# Raise and focus the client window.
#
# Both clients gate synthetic input on owning `_NET_ACTIVE_WINDOW` (the X11
# focus guard), and any other window that opens on a shared desktop mid-run
# would silently swallow the rest of a workload, so every drive step re-focuses
# first rather than assuming focus survived.
focus_client() {
  local wid
  wid="$(client_window)"
  [[ -n "$wid" ]] || return 1
  xdotool windowactivate --sync "$wid" 2>/dev/null \
    || xdotool windowfocus --sync "$wid" 2>/dev/null || true
  sleep "${1:-0.3}"
}

send_keys() {
  local wid
  wid="$(client_window)"
  [[ -n "$wid" ]] || return 1
  if [[ "$KEY_MODE" == "xtest" ]]; then
    xdotool key "$@"
  else
    xdotool key --window "$wid" "$@"
  fi
}

type_text() {
  local wid
  wid="$(client_window)"
  [[ -n "$wid" ]] || return 1
  if [[ "$KEY_MODE" == "xtest" ]]; then
    xdotool type --delay "${2:-40}" "$1"
  else
    xdotool type --window "$wid" --delay "${2:-40}" "$1"
  fi
}

# Type a shell command into the focused pane and submit it.
#
# The Enter key is always sent as its own key event: `xdotool type` does not
# deliver a trailing newline as Return to either client, so a command typed with
# `$'\n'` appended echoes onto the command line and then simply sits there. Every
# workload that depends on a command actually running goes through here.
run_command() {
  type_text "$1" "${2:-10}" || return 1
  sleep 0.2
  send_keys Return
}

# Try a tab binding once with the current KEY_MODE and wait for a session the
# rig has not seen before to become the focused one. Args: the pre-existing
# session list (comma-wrapped), chord, and whether this is the timed AI path.
# Sets OWNED_SESSION on success. The timed path additionally brackets the key
# send and first subsequent PTY-byte observation without a settle sleep.
try_new_tab() {
  local before="$1" chord="$2" timed="$3"
  local after focused bytes_before="" bytes_now="" waited=0
  local max_polls=100 poll_delay=0.1 session_seen=0
  focus_client || return 1
  if [[ "$timed" -eq 1 ]]; then
    bytes_before="$(probe_value "$PROBE_FILE" pty_bytes)"
    bytes_before="${bytes_before:-0}"
    TAB_OPEN_STARTED_NS="$(date +%s%N)"
    TAB_FIRST_PTY_NS=""
    max_polls=1000
    poll_delay=0.01
  fi
  send_keys "$chord" || return 1
  while [[ $waited -lt $max_polls ]]; do
    after="$(probe_value "$PROBE_FILE" session_ids)"
    focused="$(probe_value "$PROBE_FILE" focused_session)"
    if [[ -n "$focused" && "$focused" != "-" && "$before" != *",${focused},"* ]] \
      && [[ ",${after}," == *",${focused},"* ]]; then
      OWNED_SESSION="$focused"
      session_seen=1
    fi
    if [[ "$timed" -eq 1 && -z "$TAB_FIRST_PTY_NS" ]]; then
      bytes_now="$(probe_value "$PROBE_FILE" pty_bytes)"
      if [[ -n "$bytes_now" && "$bytes_now" -gt "$bytes_before" ]]; then
        TAB_FIRST_PTY_NS="$(date +%s%N)"
      fi
    fi
    if [[ "$session_seen" -eq 1 ]] \
      && [[ "$timed" -eq 0 || -n "$TAB_FIRST_PTY_NS" ]]; then
      return 0
    fi
    sleep "$poll_delay"
    waited=$((waited + 1))
  done
  return 1
}

# Open a tab the rig owns, recording its session id in OWNED_SESSION.
#
# This is the safety interlock for every typing workload: the rig only ever
# types into a session that appeared in the probe's session list AFTER the
# client launched and that the probe reports as focused. A silent new-tab
# failure therefore aborts the workload instead of typing into someone's pane.
#
# It is also where the key-delivery mode is chosen. The window-targeted mode is
# preferred because a stray keystroke cannot escape into another application,
# but a toolkit that reads keys through XInput2 ignores those synthetic events
# entirely, so a failed attempt falls back to XTEST before giving up. The mode
# that worked sticks for the rest of the run.
# Returns non-zero when no owned tab could be opened.
open_owned_tab() {
  local chord="${1:-ctrl+shift+t}" timed="${2:-0}"
  local before mode other
  OWNED_SESSION=""
  wait_for_attached_session || return 1
  before=",$(probe_value "$PROBE_FILE" session_ids),"
  other="xtest"
  [[ "$KEY_MODE" == "xtest" ]] && other="window"
  for mode in "$KEY_MODE" "$other"; do
    KEY_MODE="$mode"
    if try_new_tab "$before" "$chord" "$timed"; then
      CLEANUP_SESSIONS="${CLEANUP_SESSIONS} ${OWNED_SESSION}"
      if [[ "$timed" -eq 0 ]]; then
        # Let the new shell finish printing its prompt so echo timing is not
        # measured against prompt paint. The AI timer deliberately skips this.
        sleep 1.5
      fi
      return 0
    fi
  done
  log "new tab never appeared in the probe session list (tried window-targeted and XTEST key delivery)"
  return 1
}

# Close every tab this rig opened, one at a time, only ever typing `exit` into a
# session the rig itself created.
close_owned_tabs() {
  local session focused attempts
  for session in $CLEANUP_SESSIONS; do
    attempts=0
    while [[ $attempts -lt 12 ]]; do
      focused="$(probe_value "$PROBE_FILE" focused_session)"
      if [[ "$focused" == "$session" ]]; then
        send_keys ctrl+u || true
        run_command exit 20 || true
        sleep 0.8
        break
      fi
      send_keys ctrl+Next || break
      sleep 0.4
      attempts=$((attempts + 1))
    done
  done
  CLEANUP_SESSIONS=""
}

rss_kb() {
  local pid="$1"
  [[ -r "/proc/$pid/status" ]] || { echo ""; return; }
  awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status"
}

# Wait for the attached seed shell to stop producing bytes before the timer is
# armed. This settle is condition-based and happens before the measured span;
# the timed path itself ends on the first counter increment.
wait_for_pty_idle() {
  local last="" now="" stable=0 waited=0
  while [[ $waited -lt 100 ]]; do
    now="$(probe_value "$PROBE_FILE" pty_bytes)"
    if [[ -n "$now" && "$now" == "$last" ]]; then
      stable=$((stable + 1))
      [[ $stable -ge 4 ]] && return 0
    else
      stable=0
      last="$now"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  log "AI tab: seed pane did not reach an idle PTY-byte count"
  return 1
}

# Stop the marker stub with Ctrl+C, which closes the exec-backed AI session.
# The session id came from open_owned_tab's ownership interlock, so this never
# targets a pre-existing tab.
close_owned_ai_tab() {
  local waited=0 sessions
  if [[ "$(probe_value "$PROBE_FILE" focused_session)" == "$OWNED_SESSION" ]]; then
    send_keys ctrl+c || true
  fi
  while [[ $waited -lt 50 ]]; do
    sessions=",$(probe_value "$PROBE_FILE" session_ids),"
    if [[ "$sessions" != *",${OWNED_SESSION},"* ]]; then
      CLEANUP_SESSIONS=""
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  log "AI tab: marker session ${OWNED_SESSION} did not close after Ctrl+C"
  if "$SCRIBE_TEST_BIN" daemon start >/dev/null 2>&1 \
    && "$SCRIBE_TEST_BIN" session close "$OWNED_SESSION" >/dev/null 2>&1; then
    "$SCRIBE_TEST_BIN" daemon stop >/dev/null 2>&1 || true
    CLEANUP_SESSIONS=""
    return 0
  fi
  "$SCRIBE_TEST_BIN" daemon stop >/dev/null 2>&1 || true
  return 1
}

# Q6: tab-open chord to first PTY byte from the PATH marker stub. The first
# output after the pre-timer idle check comes from the stub in the disposable
# measurement profile; open_owned_tab simultaneously proves that the new,
# focused session belongs to this run.
measure_ai_tab_latency() {
  local bin="$1" elapsed
  AI_TAB_ELAPSED_MS=""
  start_client "$bin" || return 1
  if ! wait_for_attached_session || ! wait_for_pty_idle; then
    stop_client
    return 1
  fi
  if ! open_owned_tab ctrl+alt+c 1; then
    stop_client
    return 1
  fi
  elapsed="$(calc "(end - start) / 1000000" \
    "start=$TAB_OPEN_STARTED_NS" "end=$TAB_FIRST_PTY_NS")"
  close_owned_ai_tab || true
  stop_client
  AI_TAB_ELAPSED_MS="$elapsed"
}

# --------------------------------------------------------------------------
# Metric 1: startup to first frame (same-host A/B, both clients end-to-end)
# --------------------------------------------------------------------------

# Median of a space-separated list of numbers (even count: lower middle).
median() {
  printf '%s\n' "$@" | sort -n | awk '{ v[NR] = $0 } END { if (NR) print v[int((NR + 1) / 2)] }'
}

# End-to-end startup in ms from an old-client log, for binaries built before
# the shared probe carried `startup_first_frame_ms`.
#
# The old client logs `client startup timing` lines at info level from its
# first startup step through `init_gpu_and_terminal_done` (after which the
# first frame paints), so the wall-clock delta between the first line's
# timestamp and that line's timestamp is the end-to-end number. It stops at
# GPU-ready rather than first paint, so it slightly understates the client. The
# phase-scoped `total_ms` printed on the line itself only times the GPU-init
# phase and is NOT comparable to anything. Prints ms or nothing.
startup_ms_from_log() {
  local log="$1"
  [[ -f "$log" ]] || return 0
  sed 's/\x1b\[[0-9;]*m//g' "$log" | awk '
    NR == 1 { first = $1 }
    /init_gpu_and_terminal_done/ { last = $1; exit }
    END {
      if (first == "" || last == "") exit
      print (secs(last) - secs(first)) * 1000
    }
    function secs(ts,    t, parts) {
      # 2026-07-25T06:04:46.632959Z -> seconds of day (same-day samples)
      t = substr(ts, index(ts, "T") + 1)
      sub(/Z$/, "", t)
      split(t, parts, ":")
      return parts[1] * 3600 + parts[2] * 60 + parts[3]
    }' | awk '{ printf "%.0f\n", $1 }'
}

# One startup sample for either client, launched cold and killed once it has
# reported.
#
# The total comes from the shared probe's `startup_first_frame_ms`, which both
# clients latch on their first render from the instant the probe is armed (the
# first statement of `main` in each), so the two halves of the A/B are the same
# measurement rather than two different sub-spans. A binary without that key
# falls back to `startup_ms_from_log`. The GPUI client additionally writes the
# SCRIBE_GPUI_STARTUP_TIMING marker, which attributes its total to gpui's
# `cx.open_window` bring-up versus Scribe's own work.
#
# Echoes `total|bringup|scribe`; the last two are empty for a client that does
# not write the marker.
startup_sample() {
  local bin="$1"
  local marker="$WORK_DIR/startup-marker.txt"
  local probe="$WORK_DIR/startup-probe.txt"
  local log="$WORK_DIR/startup-client.log"
  rm -f "$marker" "$probe" "$log"
  SCRIBE_PERF_PROBE="$probe" SCRIBE_GPUI_STARTUP_TIMING="$marker" \
    SCRIBE_DISABLE_ANIMATIONS=1 "$bin" >"$log" 2>&1 &
  local pid=$!
  echo "$pid" >>"$WORK_DIR/pids"
  # Wait for the probe key. A probe-less client never writes one, so its
  # GPU-ready log line arms a short grace window and then ends the sample
  # instead of burning the full timeout on every run.
  local waited=0 grace=-1
  while [[ $waited -lt 250 ]]; do
    if [[ -s "$probe" ]] && grep -q '^startup_first_frame_ms=' "$probe"; then break; fi
    if [[ $grace -lt 0 ]] && [[ -f "$log" ]] \
      && grep -q 'init_gpu_and_terminal_done' "$log"; then
      grace=$waited
    fi
    [[ $grace -ge 0 && $((waited - grace)) -ge 30 ]] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
    waited=$((waited + 1))
  done
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  local total bringup scribe
  total="$(probe_value "$probe" startup_first_frame_ms)"
  [[ -n "$total" ]] || total="$(startup_ms_from_log "$log")"
  bringup="$(probe_value "$marker" gpu_bringup_ms)"
  scribe="$(probe_value "$marker" scribe_startup_ms)"
  rm -f "$marker" "$probe" "$log"
  echo "${total}|${bringup}|${scribe}"
}

# Median-of-N startup for one client. Echoes `total|bringup|scribe`, each the
# median of the samples that produced it, or `||` when nothing was measured.
measure_startup() {
  local bin="$1" i sample one_total one_bringup one_scribe
  [[ -x "$bin" ]] || { echo "||"; return; }
  local totals=() bringups=() scribes=()
  for ((i = 0; i < STARTUP_SAMPLES; i++)); do
    sample="$(startup_sample "$bin")"
    IFS='|' read -r one_total one_bringup one_scribe <<<"$sample"
    [[ -n "$one_total" ]] && totals+=("$one_total")
    [[ -n "$one_bringup" ]] && bringups+=("$one_bringup")
    [[ -n "$one_scribe" ]] && scribes+=("$one_scribe")
    sleep 1
  done
  if [[ ${#totals[@]} -eq 0 ]]; then
    log "startup: ${bin} never reported a first frame"
    echo "||"
    return
  fi
  local med_total med_bringup="" med_scribe=""
  med_total="$(median "${totals[@]}")"
  [[ ${#bringups[@]} -gt 0 ]] && med_bringup="$(median "${bringups[@]}")"
  [[ ${#scribes[@]} -gt 0 ]] && med_scribe="$(median "${scribes[@]}")"
  echo "${med_total}|${med_bringup}|${med_scribe}"
}

# --------------------------------------------------------------------------
# Metric 2: input echo latency (median round trip, milliseconds)
# --------------------------------------------------------------------------

measure_input_latency() {
  local bin="$1" i
  start_client "$bin" || { echo ""; return; }
  if ! open_owned_tab; then
    stop_client
    echo ""
    return
  fi
  # One keystroke at a time, spaced well past the expected echo, so every
  # sample is an unambiguous key -> echo pair.
  for ((i = 0; i < LATENCY_SAMPLES; i++)); do
    [[ $((i % 5)) -eq 0 ]] && focus_client 0.15
    send_keys a || break
    sleep 0.2
  done
  sleep 0.5
  local p50 samples
  p50="$(probe_value "$PROBE_FILE" input_latency_p50_ms)"
  samples="$(probe_value "$PROBE_FILE" input_samples)"
  close_owned_tabs
  stop_client
  if [[ -z "$p50" || -z "$samples" ]] || [[ "$samples" -lt 5 ]]; then
    log "input latency: only ${samples:-0} echo samples captured"
    echo ""
    return
  fi
  echo "$p50"
}

# --------------------------------------------------------------------------
# Metric 3: cat-firehose sustained drain rate (bytes per second)
# --------------------------------------------------------------------------

measure_firehose() {
  local bin="$1"
  local payload="$WORK_DIR/firehose.txt"
  if [[ ! -f "$payload" ]]; then
    # Printable, newline-dense payload: the same work an interactive `cat` of a
    # log file does, not a degenerate stream of NULs.
    yes 'perf-ab firehose payload line for the scribe client throughput gate' \
      | head -c "$((FIREHOSE_MIB * 1024 * 1024))" >"$payload" || true
  fi
  start_client "$bin" || { echo ""; return; }
  if ! open_owned_tab; then
    stop_client
    echo ""
    return
  fi
  local idle bytes_before uptime_before
  idle="$(probe_value "$PROBE_FILE" pty_bytes)"
  focus_client
  run_command "cat ${payload}" || true
  # Wait for the stream to actually start before opening the measurement
  # window, otherwise the typing and shell start-up would be counted as drain
  # time (or, worse, the drain loop would see a still counter and stop).
  local waited=0 now
  while [[ $waited -lt 120 ]]; do
    sleep 0.25
    waited=$((waited + 1))
    now="$(probe_value "$PROBE_FILE" pty_bytes)"
    [[ "$now" != "$idle" ]] && break
  done
  bytes_before="$(probe_value "$PROBE_FILE" pty_bytes)"
  uptime_before="$(probe_value "$PROBE_FILE" uptime_ms)"
  # Drain until the byte counter stops moving, or we time out.
  local last="$bytes_before" stable=0
  waited=0
  while [[ $waited -lt 240 ]]; do
    sleep 0.5
    waited=$((waited + 1))
    now="$(probe_value "$PROBE_FILE" pty_bytes)"
    if [[ "$now" == "$last" ]]; then
      stable=$((stable + 1))
      [[ $stable -ge 2 ]] && break
    else
      stable=0
    fi
    last="$now"
  done
  local bytes_after uptime_after
  bytes_after="$(probe_value "$PROBE_FILE" pty_bytes)"
  uptime_after="$(probe_value "$PROBE_FILE" uptime_ms)"
  close_owned_tabs
  stop_client
  if [[ -z "$bytes_after" || -z "$uptime_after" ]]; then echo ""; return; fi
  local delta_bytes delta_ms
  delta_bytes="$(calc "b - a" "a=${bytes_before:-0}" "b=${bytes_after}")"
  delta_ms="$(calc "b - a" "a=${uptime_before:-0}" "b=${uptime_after}")"
  if float_cmp "$delta_ms" "<=" 0 || float_cmp "$delta_bytes" "<=" 0; then echo ""; return; fi
  calc "bytes / (ms / 1000)" "bytes=$delta_bytes" "ms=$delta_ms"
}

# --------------------------------------------------------------------------
# Metric 4: steady-state RSS at MEMORY_TABS tabs (kilobytes)
# --------------------------------------------------------------------------

measure_memory() {
  local bin="$1" sessions opened
  start_client "$bin" || { echo ""; return; }
  sessions="$(probe_value "$PROBE_FILE" sessions)"
  sessions="${sessions:-0}"
  opened=0
  while [[ "$sessions" -lt "$MEMORY_TABS" && $opened -lt "$MEMORY_TABS" ]]; do
    if ! open_owned_tab; then
      break
    fi
    opened=$((opened + 1))
    sessions="$(probe_value "$PROBE_FILE" sessions)"
    sessions="${sessions:-0}"
  done
  local rss=""
  if [[ "$sessions" -ge "$MEMORY_TABS" ]]; then
    # Let the freshly spawned shells settle before sampling steady state.
    sleep 4
    rss="$(rss_kb "$CLIENT_PID")"
  else
    log "memory: only reached ${sessions} tabs (wanted ${MEMORY_TABS})"
  fi
  close_owned_tabs
  stop_client
  echo "${rss}#${sessions}"
}

# --------------------------------------------------------------------------
# Metric 5: scroll frame pacing (fps and dropped-frame percentage)
# --------------------------------------------------------------------------

# Copy the live probe report so every key of one snapshot is read from the same
# revision of the file. The rig reads three counters per boundary and the client
# rewrites the report underneath it, so reading the live file three times can
# pair a frame count with an uptime stamp from a different flush.
snapshot_probe() {
  local dest="$1"
  [[ -f "$PROBE_FILE" ]] || return 1
  cp -f "$PROBE_FILE" "$dest" 2>/dev/null || return 1
}

# Block until the probe report carries an uptime stamp newer than `$1`.
#
# The report is only rewritten when the client paints or drains PTY bytes, so a
# quiet client leaves one stamped whenever it last had something to do. Waiting
# for a rewrite is what makes a snapshot describe *now* rather than then.
wait_for_probe_rewrite() {
  local before="$1" waited=0 now
  while [[ $waited -lt 20 ]]; do
    now="$(probe_value "$PROBE_FILE" uptime_ms)"
    [[ -n "$now" && "$now" != "$before" ]] && return 0
    sleep 0.05
    waited=$((waited + 1))
  done
  return 1
}

# Drive the scroll workload on one client and echo `<fps>#<dropped_pct>`, or an
# empty string when the workload could not be measured. The compound value is
# the same shape `measure_memory` uses, so `eval_pair` can carry it.
measure_scroll() {
  local bin="$1"
  start_client "$bin" || { echo ""; return; }
  if ! open_owned_tab; then
    stop_client
    echo ""
    return
  fi
  # Scrolling is driven INSIDE the pane, by a writer the rig starts once and then
  # leaves alone, rather than through a client-side scrollback binding or a
  # stream of synthetic keys. Both clients then see the same thing — PTY output
  # scrolling the grid — independently of which one has wired its own scroll
  # actions, and nothing between the shell and the renderer paces the frames.
  focus_client
  run_command "$SCROLL_COMMAND" || true
  sleep "$SCROLL_SETTLE_SECONDS"
  # The window is derived from the probe's own uptime stamps, so both edges have
  # to name a moment the client was actually busy: the report is rewritten when
  # the client paints or drains bytes and at no other time, so a snapshot taken
  # while it is quiet carries a stamp from whenever it last had work. Waiting for
  # a rewrite before opening the window is also the check that the writer is
  # actually running — if it never started, there is nothing to measure.
  local stale_stamp
  stale_stamp="$(probe_value "$PROBE_FILE" uptime_ms)"
  if ! wait_for_probe_rewrite "$stale_stamp"; then
    log "scroll: the pane produced no output, so there was no scrolling to measure"
    close_owned_tabs
    stop_client
    echo ""
    return
  fi
  local before_snap="$WORK_DIR/scroll-before.txt"
  snapshot_probe "$before_snap" || { stop_client; echo ""; return; }
  local frames_before dropped_before uptime_before
  frames_before="$(probe_value "$before_snap" frames)"
  dropped_before="$(probe_value "$before_snap" dropped_frames)"
  uptime_before="$(probe_value "$before_snap" uptime_ms)"
  sleep "$SCROLL_SECONDS"
  local after_snap="$WORK_DIR/scroll-after.txt"
  snapshot_probe "$after_snap" || { stop_client; echo ""; return; }
  local frames_after dropped_after uptime_after
  frames_after="$(probe_value "$after_snap" frames)"
  dropped_after="$(probe_value "$after_snap" dropped_frames)"
  uptime_after="$(probe_value "$after_snap" uptime_ms)"
  # Stop the writer before the tab is closed, so `exit` reaches the shell.
  focus_client 0.1
  send_keys ctrl+c || true
  sleep 0.5
  close_owned_tabs
  stop_client
  if [[ -z "$frames_after" || -z "$uptime_after" ]]; then echo ""; return; fi
  local delta_frames delta_dropped delta_ms
  delta_frames="$(calc "b - a" "a=${frames_before:-0}" "b=${frames_after}")"
  delta_dropped="$(calc "b - a" "a=${dropped_before:-0}" "b=${dropped_after}")"
  delta_ms="$(calc "b - a" "a=${uptime_before:-0}" "b=${uptime_after}")"
  if float_cmp "$delta_ms" "<=" 0 || float_cmp "$delta_frames" "<=" 0; then
    log "scroll: the client painted no frames while the grid was scrolling (${delta_frames} frames over ${delta_ms} ms)"
    echo ""
    return
  fi
  local fps dropped_pct
  fps="$(calc "f / (ms / 1000)" "f=$delta_frames" "ms=$delta_ms")"
  dropped_pct="$(calc "d * 100 / (f + d)" "d=$delta_dropped" "f=$delta_frames")"
  echo "${fps}#${dropped_pct}"
}

# --------------------------------------------------------------------------
# Metric evaluation: measure, compare, verdict
# --------------------------------------------------------------------------

LIVE_BLOCKER=""

live_ready() {
  [[ "$MODE" == "live" && -z "$LIVE_BLOCKER" ]]
}

eval_startup() {
  local new_split="" old_split=""
  if live_ready && metric_enabled startup && [[ -n "$NEW_CLIENT" ]]; then
    log "measuring startup: new client (median of ${STARTUP_SAMPLES})"
    new_split="$(measure_startup "$NEW_CLIENT")"
  fi
  local new_total="" bringup="" scribe_ms=""
  if [[ -n "$new_split" ]]; then
    IFS='|' read -r new_total bringup scribe_ms <<<"$new_split"
  fi
  if live_ready && metric_enabled startup && [[ -n "$OLD_CLIENT" ]]; then
    log "measuring startup: old client (median of ${STARTUP_SAMPLES})"
    old_split="$(measure_startup "$OLD_CLIENT")"
    local old_total="${old_split%%|*}"
    if [[ -n "$old_total" ]]; then
      OLD_STARTUP_MS="$old_total"
      record_baseline startup_first_frame_ms "$old_total"
    fi
  fi

  if [[ -z "$new_total" ]]; then
    STARTUP_STATUS="NOT-MEASURED"
    STARTUP_VALUE="not captured"
    STARTUP_NOTE="Not captured: ${LIVE_BLOCKER:-run with --live --new-client <bin>.} A client reports the span through SCRIBE_PERF_PROBE, or through its startup log when the binary predates that probe key."
    return
  fi

  STARTUP_VALUE="${new_total} ms total"
  [[ -n "$scribe_ms" ]] && STARTUP_VALUE="${STARTUP_VALUE} (${scribe_ms} ms Scribe + ${bringup} ms gpui GPU bring-up)"

  # 1a: the part this repo controls, gated absolutely. Worst status wins, so a
  # metric that could not be attributed can never leave the gate at PASS.
  local scribe_verdict scribe_status
  if [[ -z "$scribe_ms" ]]; then
    scribe_verdict="not attributed: the client wrote no gpu_bringup_ms marker"
    scribe_status="NOT-MEASURED"
  elif float_cmp "$scribe_ms" "<=" "$SCRIBE_STARTUP_BUDGET_MS"; then
    scribe_verdict="PASS (<= ${SCRIBE_STARTUP_BUDGET_MS} ms)"
    scribe_status="PASS"
  else
    scribe_verdict="FAIL (> ${SCRIBE_STARTUP_BUDGET_MS} ms)"
    scribe_status="FAIL"
  fi

  # 1b: the whole span, compared against the old client measured the same way.
  local total_verdict total_status ceiling
  if [[ -z "${OLD_STARTUP_MS:-}" ]]; then
    total_verdict="no baseline to compare against"
    total_status="NO-BASELINE"
  else
    ceiling="$(calc "old * (1 + pct / 100)" "old=$OLD_STARTUP_MS" "pct=$NOISE_TOLERANCE_PCT")"
    if float_cmp "$new_total" "<=" "$ceiling"; then
      total_verdict="PASS (<= ${ceiling} ms)"
      total_status="PASS"
    else
      total_verdict="FAIL (> ${ceiling} ms)"
      total_status="FAIL"
    fi
  fi

  if [[ "$scribe_status" == "FAIL" || "$total_status" == "FAIL" ]]; then
    STARTUP_STATUS="FAIL"
  elif [[ "$scribe_status" == "PASS" && "$total_status" == "PASS" ]]; then
    STARTUP_STATUS="PASS"
  elif [[ "$total_status" == "NO-BASELINE" ]]; then
    STARTUP_STATUS="NO-BASELINE"
  else
    STARTUP_STATUS="NOT-MEASURED"
  fi

  STARTUP_NOTE="Q3 re-scope (2026-07-24) plus its absolute half (2026-07-25, bead scribe-38e.83); the retired 500 ms ceiling is below this platform's GPU bring-up floor for both clients. (1a) Scribe-attributable startup ${scribe_ms:-n/a} ms against a ${SCRIBE_STARTUP_BUDGET_MS} ms budget: ${scribe_verdict}. (1b) Total first frame ${new_total} ms against the old client's ${OLD_STARTUP_MS:-unrecorded} ms with the ${NOISE_TOLERANCE_PCT}% noise allowance: ${total_verdict}. Method: median of ${STARTUP_SAMPLES} cold launches per client; the span is the first painted frame minus the probe arm (the first statement of each client's main), falling back to the startup-log wall clock for a binary without that probe key. The gpui split comes from the client's SCRIBE_GPUI_STARTUP_TIMING marker, which times \`cx.open_window\` — the span in which no Scribe code runs. Splash deletion (OQ8) stays authorized while this PASSes."
}

# Shared shape for the metrics measured on both clients: measure the new client,
# measure (or read) the old client, then hand both back as `new|old`.
#
# Args: label measure_fn baseline_key only_metric_name
eval_pair() {
  local metric="$1" fn="$2" key="$3" only="$4"
  local new_value="" old_value=""
  if live_ready && metric_enabled "$only" && [[ -n "$NEW_CLIENT" ]]; then
    log "measuring ${metric}: new client"
    new_value="$($fn "$NEW_CLIENT")"
  fi
  if live_ready && metric_enabled "$only" && [[ -n "$OLD_CLIENT" ]]; then
    log "measuring ${metric}: old client"
    old_value="$($fn "$OLD_CLIENT")"
  fi
  if [[ -z "$old_value" ]]; then
    old_value="$(baseline_value "$key")"
  else
    record_baseline "$key" "$old_value"
  fi
  echo "${new_value}|${old_value}"
}

eval_latency() {
  local pair
  pair="$(eval_pair input-latency measure_input_latency input_latency_p50_ms latency)"
  LAT_NEW="${pair%%|*}"
  LAT_OLD="${pair##*|}"
  LAT_METHOD="Median of ${LATENCY_SAMPLES} instrumented key -> PTY-echo round trips in a rig-owned tab, both clients measured by the shared probe. PASS when the new median is within ${NOISE_TOLERANCE_PCT}% of the old one."
  LAT_VALUE="not captured"
  if [[ -z "$LAT_NEW" ]]; then
    LAT_STATUS="NOT-MEASURED"; return
  fi
  LAT_VALUE="${LAT_NEW} ms"
  if [[ -z "$LAT_OLD" ]]; then
    LAT_STATUS="NO-BASELINE"; return
  fi
  local ceiling
  ceiling="$(calc "old * (1 + pct / 100)" "old=$LAT_OLD" "pct=$NOISE_TOLERANCE_PCT")"
  if float_cmp "$LAT_NEW" "<=" "$ceiling"; then LAT_STATUS="PASS"; else LAT_STATUS="FAIL"; fi
}

eval_firehose() {
  local pair
  pair="$(eval_pair firehose-throughput measure_firehose firehose_bytes_per_sec firehose)"
  FIRE_NEW="${pair%%|*}"
  FIRE_OLD="${pair##*|}"
  FIRE_METHOD="Sustained bytes/sec the client drains while \`cat\`ting a ${FIREHOSE_MIB} MiB file in a rig-owned tab, counted at each client's PTY-output entry point. PASS when the new rate is within ${NOISE_TOLERANCE_PCT}% of the old rate."
  FIRE_VALUE="not captured"
  if [[ -z "$FIRE_NEW" ]]; then
    FIRE_STATUS="NOT-MEASURED"; return
  fi
  FIRE_VALUE="$(calc "b / 1048576" "b=$FIRE_NEW") MiB/s"
  if [[ -z "$FIRE_OLD" ]]; then
    FIRE_STATUS="NO-BASELINE"; return
  fi
  local floor
  floor="$(calc "old * (1 - pct / 100)" "old=$FIRE_OLD" "pct=$NOISE_TOLERANCE_PCT")"
  if float_cmp "$FIRE_NEW" ">=" "$floor"; then FIRE_STATUS="PASS"; else FIRE_STATUS="FAIL"; fi
}

eval_memory() {
  local pair new_raw old_raw
  pair="$(eval_pair "memory-${MEMORY_TABS}-tabs" measure_memory memory_rss_kb memory)"
  new_raw="${pair%%|*}"
  old_raw="${pair##*|}"
  # measure_memory returns `<rss_kb>#<tabs>`; a committed baseline is bare kB.
  MEM_NEW="${new_raw%%#*}"
  MEASURED_TABS="${new_raw##*#}"
  [[ "$MEASURED_TABS" == "$new_raw" ]] && MEASURED_TABS=""
  MEM_OLD="${old_raw%%#*}"
  MEM_METHOD="Steady-state VmRSS from /proc once the rig has opened tabs up to ${MEMORY_TABS}, sampled identically for both clients. PASS when the new RSS is at most old + ${MEM_REGRESSION_MAX_PCT}%."
  MEM_VALUE="not captured"
  if [[ -z "$MEM_NEW" ]]; then
    MEM_STATUS="NOT-MEASURED"; return
  fi
  MEM_VALUE="$(calc "kb / 1024" "kb=$MEM_NEW") MiB"
  if [[ -z "$MEM_OLD" ]]; then
    MEM_STATUS="NO-BASELINE"; return
  fi
  local ceiling
  ceiling="$(calc "old * (1 + pct / 100)" "old=$MEM_OLD" "pct=$MEM_REGRESSION_MAX_PCT")"
  if float_cmp "$MEM_NEW" "<=" "$ceiling"; then MEM_STATUS="PASS"; else MEM_STATUS="FAIL"; fi
}

# Metric 5. The threshold is absolute, so unlike the other four the old client's
# number is not a comparison input — it is the attribution: a frame-pacing target
# the old client also misses on the same workload is a property of the workload
# rather than of the client under test, and the rig used to have no way to tell
# those apart (bead scribe-38e.91).
eval_scroll() {
  local pair new_raw old_raw
  pair="$(eval_pair scroll-fps measure_scroll scroll_fps scroll)"
  new_raw="${pair%%|*}"
  old_raw="${pair##*|}"
  # `measure_scroll` returns `<fps>#<dropped_pct>`; a committed baseline stores
  # the two halves in separate slots, so a bare value came from the file.
  SCROLL_NEW_FPS="${new_raw%%#*}"
  SCROLL_NEW_DROPPED="${new_raw##*#}"
  [[ "$SCROLL_NEW_DROPPED" == "$new_raw" ]] && SCROLL_NEW_DROPPED=""
  SCROLL_OLD_FPS="${old_raw%%#*}"
  SCROLL_OLD_DROPPED="${old_raw##*#}"
  if [[ "$SCROLL_OLD_DROPPED" == "$old_raw" ]]; then
    SCROLL_OLD_DROPPED="$(baseline_value scroll_dropped_pct)"
  else
    record_baseline scroll_dropped_pct "$SCROLL_OLD_DROPPED"
  fi

  SCROLL_OLD_VALUE="unrecorded"
  [[ -n "$SCROLL_OLD_FPS" ]] && SCROLL_OLD_VALUE="${SCROLL_OLD_FPS} fps, ${SCROLL_OLD_DROPPED:-?}% dropped"

  SCROLL_METHOD="The grid is scrolled for ${SCROLL_SECONDS}s by an unpaced writer running inside a rig-owned pane (\`${SCROLL_COMMAND}\`, measured after a ${SCROLL_SETTLE_SECONDS}s settle), so the workload is identical on both clients, independent of client-side scrollback bindings, and paced by nothing between the shell and the renderer; fps and dropped frames come from the shared probe's frame-gap accounting over that window. PASS at sustained ${SCROLL_TARGET_FPS} fps (within ${NOISE_TOLERANCE_PCT}%) with < ${SCROLL_DROPPED_MAX_PCT}% dropped. The old client's number is the attribution for a failure, not a threshold: the target is absolute."
  SCROLL_VALUE="not captured"
  if [[ -z "$SCROLL_NEW_FPS" || -z "$SCROLL_NEW_DROPPED" ]]; then
    SCROLL_STATUS="NOT-MEASURED"; return
  fi
  SCROLL_VALUE="${SCROLL_NEW_FPS} fps, ${SCROLL_NEW_DROPPED}% dropped"
  local floor
  floor="$(calc "target * (1 - pct / 100)" "target=$SCROLL_TARGET_FPS" "pct=$NOISE_TOLERANCE_PCT")"
  if float_cmp "$SCROLL_NEW_FPS" ">=" "$floor" \
    && float_cmp "$SCROLL_NEW_DROPPED" "<" "$SCROLL_DROPPED_MAX_PCT"; then
    SCROLL_STATUS="PASS"
  else
    SCROLL_STATUS="FAIL"
  fi
}

# --------------------------------------------------------------------------
# Report generation
# --------------------------------------------------------------------------

run_ai_tab_only() {
  [[ "$MODE" == "live" ]] || die \
    "--ai-tab-only is a timed runtime measurement and requires --live."
  [[ -n "$NEW_CLIENT" ]] || die \
    "--ai-tab-only requires --new-client <bin>." \
    "Pass a probe-enabled client built from this tree."

  local blocker stub verdict
  blocker="$(live_blocker)"
  [[ -z "$blocker" ]] || die "AI-tab measurement blocked: ${blocker}"
  stub="$(ai_tab_stub || true)"
  [[ -n "$stub" ]] || die \
    "--ai-tab-only requires a marker stub named claude on PATH." \
    "The executable must contain the literal ${AI_TAB_STUB_MARKER}, print that" \
    "marker immediately, and remain alive until the rig sends Ctrl+C."
  log "AI tab: marker stub ${stub}"

  live_preflight
  measure_ai_tab_latency "$NEW_CLIENT" || die \
    "AI-tab latency was not captured." \
    "The marker must be the first output in the disposable login profile, and" \
    "the ctrl+alt+c tab must appear in the runtime probe before the timeout."

  verdict="FAIL"
  if float_cmp "$AI_TAB_ELAPSED_MS" "<=" "$AI_TAB_BUDGET_MS"; then
    verdict="PASS"
  fi
  echo "ai_tab_open_to_first_pty_byte_ms=${AI_TAB_ELAPSED_MS}"
  echo "ai_tab_budget_ms=${AI_TAB_BUDGET_MS}"
  echo "ai_tab_budget_verdict=${verdict}"
  [[ "$verdict" == "PASS" ]]
}

overall_verdict() {
  local status
  for status in "$STARTUP_STATUS" "$LAT_STATUS" "$FIRE_STATUS" "$MEM_STATUS" "$SCROLL_STATUS"; do
    if [[ "$status" == "FAIL" ]]; then echo "FAIL"; return; fi
  done
  for status in "$STARTUP_STATUS" "$LAT_STATUS" "$FIRE_STATUS" "$MEM_STATUS" "$SCROLL_STATUS"; do
    if [[ "$status" != "PASS" ]]; then echo "INCOMPLETE"; return; fi
  done
  echo "PASS"
}

emit_report() {
  local now
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  if [[ "$MODE" == "live" ]]; then
    LIVE_BLOCKER="$(live_blocker)"
    if [[ -n "$LIVE_BLOCKER" ]]; then
      # An environment that cannot host a GUI client at all (CI, no server) is
      # reported as NOT-MEASURED rather than being fatal; unusable *inputs* are
      # fatal, and that is what live_preflight checks.
      log "live mode blocked: $LIVE_BLOCKER"
    else
      live_preflight
    fi
  else
    LIVE_BLOCKER="assess mode never launches a GUI; re-run with --live."
  fi

  OLD_STARTUP_MS="$(baseline_value startup_first_frame_ms)"

  eval_startup
  eval_latency
  eval_firehose
  eval_memory
  eval_scroll

  local overall
  overall="$(overall_verdict)"

  local old_lat old_fire old_mem
  old_lat="unrecorded"
  [[ -n "$LAT_OLD" ]] && old_lat="${LAT_OLD} ms"
  old_fire="unrecorded"
  [[ -n "$FIRE_OLD" ]] && old_fire="$(calc "b / 1048576" "b=$FIRE_OLD") MiB/s"
  old_mem="unrecorded"
  [[ -n "$MEM_OLD" ]] && old_mem="$(calc "kb / 1024" "kb=$MEM_OLD") MiB"

  cat >"$OUT" <<EOF
# GPUI Client Perf A/B Report

Generated by \`tools/perf-ab-rig/run-perf-ab.sh\` (mode: \`${MODE}\`) at ${now}.
This is the launch-blocking performance comparison for the GPUI client rebuild
(beads scribe-38e.41 / scribe-38e.51), gating cutover in the launch go/no-go
(scribe-38e.42).

## Thresholds (Clarification Q3, startup re-scoped 2026-07-24)

| Metric | Threshold |
|---|---|
| Scribe-attributable startup | <= ${SCRIBE_STARTUP_BUDGET_MS} ms absolute |
| Startup to first frame (total) | no worse than old client end-to-end (${NOISE_TOLERANCE_PCT}% noise allowance, median of ${STARTUP_SAMPLES}) |
| Input latency | no worse than old client (${NOISE_TOLERANCE_PCT}% noise allowance) |
| cat-firehose throughput | no worse than old client (${NOISE_TOLERANCE_PCT}% noise allowance) |
| Memory at ${MEMORY_TABS} tabs | <= old client + ${MEM_REGRESSION_MAX_PCT}% |
| Scroll | sustained ${SCROLL_TARGET_FPS} fps, < ${SCROLL_DROPPED_MAX_PCT}% dropped |

The startup threshold was re-scoped on 2026-07-24 and amended by bead
scribe-38e.83: the original 500 ms absolute ceiling to first frame is below the
GPU bring-up floor of the host for *both* clients, so it is replaced by an
absolute budget on the startup work this repo controls plus a like-for-like
comparison of the whole span.

Old-client baselines come from \`${BASELINE#"$REPO_ROOT/"}\`. \`--live --old-client <bin>\`
re-measures them with the same probe in the same session, and
\`--record-baseline\` writes the measured values back into that file.

## Results

| Metric | New client | Old client | Verdict |
|---|---|---|---|
| Startup to first frame (end-to-end) | ${STARTUP_VALUE} | ${OLD_STARTUP_MS:-unrecorded} ms | ${STARTUP_STATUS} |
| Input latency (p50 echo) | ${LAT_VALUE} | ${old_lat} | ${LAT_STATUS} |
| cat-firehose throughput | ${FIRE_VALUE} | ${old_fire} | ${FIRE_STATUS} |
| Memory at ${MEMORY_TABS} tabs | ${MEM_VALUE} | ${old_mem} | ${MEM_STATUS} |
| Scroll fps / dropped frames | ${SCROLL_VALUE} | ${SCROLL_OLD_VALUE} | ${SCROLL_STATUS} |

**Overall gate verdict: ${overall}.** \`PASS\` requires all five metrics
measured and inside their thresholds. \`INCOMPLETE\` means at least one metric
could not be captured (no display, no server, missing binary, or no committed
baseline to compare against); the gate cannot pass on it. A \`FAIL\` reopens the
perf bead.

## Per-metric detail

### Startup to first frame -- ${STARTUP_STATUS}
${STARTUP_NOTE}

### Input latency -- ${LAT_STATUS}
${LAT_METHOD}

### cat-firehose throughput -- ${FIRE_STATUS}
${FIRE_METHOD}

### Memory at ${MEMORY_TABS} tabs -- ${MEM_STATUS}
${MEM_METHOD}${MEASURED_TABS:+ Reached ${MEASURED_TABS} tabs.}

### Scroll fps / dropped frames -- ${SCROLL_STATUS}
${SCROLL_METHOD}

## Reproducing

Assess (safe, no GUI, current-state report):

    tools/perf-ab-rig/run-perf-ab.sh

Launch gate (full A/B on the same machine/session, attaches to the running
\`${DEV_EXE_STEM}\` server, never restarts it):

    tools/perf-ab-rig/run-perf-ab.sh --live \\
      --new-client target/release/scribe-client \\
      --old-client target/release/scribe-client \\
      --scribe-test target/release/scribe-test --record-baseline

Every binary above must come from this tree. The installed
\`/usr/bin/scribe-client\` predates the shared probe and can never write a probe
report, so a run pointed at it aborts in the preflight instead of timing out
each workload; \`scribe-test\` seeds the session the client attaches to and a
full \`--live\` run refuses to start without it.

A live run seeds sessions, opens tabs and types into them, so it is staged onto
the isolated \`${DEV_EXE_STEM}\` install and never touches the stable server:
every binary it launches is copied to a path whose file stem is
\`${DEV_EXE_STEM}\`, which is what selects that runtime slug. Install and start
that server once with \`just install-dev\`.

Both clients are instrumented by the shared runtime probe
(\`crates/scribe-common/src/perf_probe.rs\`), armed by \`${PROBE_ENV_KEY}\`;
the GPUI client additionally writes the first-frame marker named by
\`SCRIBE_GPUI_STARTUP_TIMING\`. Every typing workload runs in a tab the rig
opened and verified through the probe's session list, so it never types into a
pane that was already open.
EOF

  echo "wrote report: $OUT"
  echo "overall gate verdict: $overall"
  [[ "$overall" == "FAIL" ]] && return 1
  return 0
}

if [[ "$AI_TAB_ONLY" -eq 1 ]]; then
  run_ai_tab_only
else
  emit_report
fi
