#!/bin/bash
# @lat: [[test#Visual E2E Tests#Geometry capture survives a resize]]
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Probe: does the persisted geometry record hold the window's ROOT position?
#
# `layout-restore.sh` only ever MOVES the window. A reparenting window manager
# answers a move with the synthetic ConfigureNotify ICCCM requires, which
# carries root-relative coordinates — so that path is the one case that cannot
# expose a parent-relative reading. A RESIZE is different: the X server sends
# the client a real ConfigureNotify whose x/y are relative to the WM frame, and
# GPUI's X11 backend stores `event.x`/`event.y` verbatim
# (gpui_linux x11/client.rs, the ConfigureNotify arm) without checking whether
# the event was synthetic. If that reading wins, `capture_geometry` persists the
# frame's border/titlebar offset instead of the window's position on the
# desktop — and the next start restores the window to the top-left of the
# primary monitor whatever screen it was on.
#
# Phases:
#   0. baseline — move the window, the record must match xdotool;
#   1. resize the window, the record must STILL match xdotool;
#   2. restart, the window must come back where phase 1 left it.
#
# Requires: visual container; xdotool, python3.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
GEOMETRY_DIR="$STATE_DIR/windows"

MOVE_X=320
MOVE_Y=180

fail() {
    echo "FAIL: $1" >&2
    echo "--- geometry records ---" >&2
    for f in "$GEOMETRY_DIR"/*.toml; do
        echo "  $f" >&2
        sed 's/^/    /' "$f" >&2
    done
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" >&2 || true
    exit 1
}

list_windows() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}
find_window() { list_windows | tail -1; }

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.8
}

# Root-relative position and size of the live window, per xdotool (which
# resolves through TranslateCoordinates, so it is always root-relative).
live_geometry() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || return 1
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    printf '%s %s %s %s' "$X" "$Y" "$WIDTH" "$HEIGHT"
}

# The newest persisted record, as `x y width height maximized`.
record_geometry() {
    local newest
    newest=$(ls -t "$GEOMETRY_DIR"/*.toml 2>/dev/null | head -1)
    [ -n "$newest" ] || return 1
    python3 - "$newest" <<'PY'
import sys
values = {}
for line in open(sys.argv[1]):
    if "=" in line:
        key, _, raw = line.partition("=")
        values[key.strip()] = raw.strip()
print(values.get("x"), values.get("y"), values.get("width"),
      values.get("height"), values.get("maximized"))
PY
}

wait_for_client_exit() {
    local timeout_secs="$1" started
    started=$(date +%s)
    while pgrep -f 'scribe-client' >/dev/null 2>&1; do
        [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ] && return 1
        sleep 0.3
    done
    return 0
}

launch_client() {
    scribe-client >>"$CLIENT_LOG" 2>&1 &
    xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
    sleep 3
}

sleep 1.0
focus

# The record is the frame origin while xdotool reports the content origin, so
# the two differ by one decoration. That offset is a CONSTANT the restore path
# already compensates for; what matters is whether it stays constant.
delta() {
    local live rec
    live=$(live_geometry) || return 1
    rec=$(record_geometry) || return 1
    python3 - "$live" "$rec" <<'PY'
import sys
lx, ly = sys.argv[1].split()[:2]
rx, ry = sys.argv[2].split()[:2]
print(int(lx) - int(rx), int(ly) - int(ry))
PY
}

# ── Phase 0: baseline — the offset after a MOVE ───────────────────
WID=$(find_window)
xdotool windowmove "$WID" "$MOVE_X" "$MOVE_Y"
# Past RESTORE_DEBOUNCE (500 ms) plus the lifecycle tick.
sleep 3.0
LIVE_MOVED=$(live_geometry)
REC_MOVED=$(record_geometry)
DELTA_MOVED=$(delta)
echo "PHASE 0: live [$LIVE_MOVED]  record [$REC_MOVED]  offset ($DELTA_MOVED)"
LIVE_MX=$(echo "$LIVE_MOVED" | cut -d' ' -f1)
LIVE_MY=$(echo "$LIVE_MOVED" | cut -d' ' -f2)
echo "PHASE 0 PASS: after a move the record trails the window by ($DELTA_MOVED)"

# ── Phase 1: a RESIZE must not change that offset ─────────────────
# The window is not moved here, so the recorded position must not move either:
# the offset has to stay exactly what phase 0 measured. If the capture takes a
# parent-relative ConfigureNotify, the record collapses to the frame's border
# and titlebar and the offset jumps to the window's whole root position.
xdotool windowsize "$WID" 900 620
sleep 3.0
LIVE_SIZED=$(live_geometry)
REC_SIZED=$(record_geometry)
DELTA_SIZED=$(delta)
echo "PHASE 1: live [$LIVE_SIZED]  record [$REC_SIZED]  offset ($DELTA_SIZED)"
LIVE_SX=$(echo "$LIVE_SIZED" | cut -d' ' -f1)
LIVE_SY=$(echo "$LIVE_SIZED" | cut -d' ' -f2)
[ "$DELTA_SIZED" = "$DELTA_MOVED" ] \
    || fail "PHASE 1: a RESIZE changed the record-to-window offset from ($DELTA_MOVED) to ($DELTA_SIZED) — the capture took a parent-relative ConfigureNotify"
echo "PHASE 1 PASS: a resize left the offset at ($DELTA_SIZED)"

# ── Phase 2: a restart lands where phase 1 left the window ────────
pkill -TERM -f 'scribe-client' || true
wait_for_client_exit 20 || fail "PHASE 2: the client did not exit"
launch_client
focus
LIVE_BACK=$(live_geometry)
BACK_X=$(echo "$LIVE_BACK" | cut -d' ' -f1)
BACK_Y=$(echo "$LIVE_BACK" | cut -d' ' -f2)
echo "PHASE 2: live [$LIVE_BACK], wanted ($LIVE_SX,$LIVE_SY)"
[ "$BACK_X" = "$LIVE_SX" ] && [ "$BACK_Y" = "$LIVE_SY" ] \
    || fail "PHASE 2: after a resize-then-restart the window came back at ($BACK_X,$BACK_Y), expected ($LIVE_SX,$LIVE_SY)"
echo "PHASE 2 PASS: the window came back at ($BACK_X,$BACK_Y)"

echo ""
echo "PASS: geometry capture probe"
