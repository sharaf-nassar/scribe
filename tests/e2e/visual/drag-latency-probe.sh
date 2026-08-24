#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# DISPOSABLE probe for scribe-07xb.10 (US3 P4 budget): input-to-paint latency of
# the workspace-pill drag overlay.
#
# Relaunches the client with SCRIBE_DRAG_PROBE=1, splits the window into two
# workspace regions, sweeps the left region's pill across the right region so
# every zone boundary is crossed, cancels with Escape, then reduces the
# `input_to_paint_us` samples the probe logged into a distribution + p95.
#
# Not registered in the justfile suite inventory: it measures a hardware profile
# rather than asserting a contract, and Lavapipe under Xvfb is not that profile.
# Run it as `just e2e-visual drag-latency-probe.sh` and read the p95 line.
#
# Requires: visual container; xdotool.
# e2e-timeout: 240
set -e

CLIENT_LOG=/output/drag-probe-client.log

fail() {
    echo "FAIL: $1" >&2
    tail -40 "$CLIENT_LOG" >&2 2>/dev/null || true
    exit 1
}

scribe_windows() {
    { xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null || true
      xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
    } | sort -u
}

plain_client_log() {
    sed $'s/\033\\[[0-9;]*m//g' "$CLIENT_LOG"
}

# Restart the client under the probe env: the entrypoint launched it without
# SCRIBE_DRAG_PROBE, and the switch is read once at window construction.
#
# RUST_LOG must name the probe target explicitly: the entrypoint exports
# `scribe_server=info,scribe_client=info`, and an EnvFilter with only those
# directives drops every `scribe::drag_probe` event — the armed trace and the
# input_to_paint_us samples this probe greps for would never reach the log.
pkill -f 'scribe-client' 2>/dev/null || true
sleep 1.5
SCRIBE_DRAG_PROBE=1 RUST_LOG='scribe_client=info,scribe::drag_probe=info' \
    scribe-client >"$CLIENT_LOG" 2>&1 &
PROBE_PID=$!
trap 'kill "$PROBE_PID" 2>/dev/null || true' EXIT
tr '\0' '\n' < "/proc/$PROBE_PID/environ" | grep -q '^SCRIBE_DRAG_PROBE=1$' \
    || fail "probe subprocess did not inherit SCRIBE_DRAG_PROBE=1"

for _ in $(seq 1 60); do
    [ -n "$(scribe_windows)" ] && break
    sleep 0.5
done
WID=$(scribe_windows | tail -1)
[ -n "$WID" ] || fail "the probe client never mapped a window"

xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID" || true
sleep 1.5
INFO=$(xwininfo -id "$WID")
WIN_X=$(printf '%s\n' "$INFO" | awk '/Absolute upper-left X/ { print $4 }')
WIN_Y=$(printf '%s\n' "$INFO" | awk '/Absolute upper-left Y/ { print $4 }')
WIN_W=$(printf '%s\n' "$INFO" | awk '/Width:/ { print $2 }')
WIN_H=$(printf '%s\n' "$INFO" | awk '/Height:/ { print $2 }')
echo "client ${WIN_W}x${WIN_H} at ${WIN_X},${WIN_Y}"

# Two regions: the left one owns the pill this probe drags, the right one owns
# the five drop zones the sweep crosses.
xdotool key --clearmodifiers ctrl+alt+backslash
sleep 2.0
plain_client_log | grep -qE 'split the window into a new workspace region.*regions=2' \
    || fail "ctrl+alt+backslash did not produce two workspace regions"

# Measure the live pill's hit target instead of trusting text-width arithmetic:
# click-test the first titlebar run until the gated arm trace proves which inset
# is inside the label (and not an optional nested Beads button).
xdotool windowactivate --sync "$WID" 2>/dev/null || true
xdotool windowfocus --sync "$WID" 2>/dev/null || true
sleep 0.4
echo "probe window $WID active $(xdotool getactivewindow 2>/dev/null || true) focus $(xdotool getwindowfocus 2>/dev/null || true)"
PILL_INSET=""
PILL_Y_OFFSET=""
for yoff in $(seq -16 4 52); do
    for inset in $(seq 4 8 92); do
        xdotool mousemove --sync "$(( WIN_X + inset ))" "$(( WIN_Y + yoff ))"
        xdotool mousedown 1
        sleep 0.08
        xdotool mouseup 1
        sleep 0.08
        if plain_client_log | grep -q 'workspace drag armed'; then
            PILL_INSET="$inset"
            PILL_Y_OFFSET="$yoff"
            break 2
        fi
    done
done
[ -n "$PILL_INSET" ] || fail "could not measure a live workspace-pill hit target"
PILL_X=$(( WIN_X + PILL_INSET ))
PILL_Y=$(( WIN_Y + PILL_Y_OFFSET ))
echo "measured pill offset $PILL_INSET,$PILL_Y_OFFSET"
SWEEP_Y=$(( WIN_Y + WIN_H / 2 ))
SWEEP_FROM=$(( WIN_X + WIN_W / 2 + 20 ))
SWEEP_TO=$(( WIN_X + WIN_W - 40 ))
STEP=6

xdotool mousemove --sync --window "$WID" "$PILL_INSET" "$PILL_Y_OFFSET"
echo "pill target $(xdotool getmouselocation --shell | tr '\n' ' ')"
sleep 0.4
xdotool mousedown 1
sleep 0.2
# Past GPUI's ~2px native drag threshold, then down into the grid.
xdotool mousemove_relative -- 3 0
sleep 0.2
xdotool mousemove --sync $(( PILL_X + 8 )) "$SWEEP_Y"
sleep 0.3

# Sweep right and back: crossing the right region's left band, centre and right
# band means every zone highlight fades in and out at least twice.
for pass in 1 2; do
    for x in $(seq "$SWEEP_FROM" "$STEP" "$SWEEP_TO"); do
        xdotool mousemove --sync "$x" "$SWEEP_Y"
        sleep 0.01
    done
    for x in $(seq "$SWEEP_TO" "-$STEP" "$SWEEP_FROM"); do
        xdotool mousemove --sync "$x" "$SWEEP_Y"
        sleep 0.01
    done
    echo "sweep pass $pass done"
done

# Escape cancels: zero protocol frames, zero tree change, ghost snaps back.
xdotool key --clearmodifiers Escape
sleep 0.5
xdotool mouseup 1
sleep 1.0

SAMPLES=/output/drag-probe-samples.txt
plain_client_log | grep -o 'input_to_paint_us=[0-9]*' | cut -d= -f2 > "$SAMPLES" || true
COUNT=$(wc -l < "$SAMPLES")
[ "$COUNT" -ge 50 ] || fail "only $COUNT probe samples; the press never armed the pill drag"

python3 - "$SAMPLES" <<'PY'
import sys

samples = sorted(int(line) / 1000.0 for line in open(sys.argv[1]) if line.strip())


def pct(p):
    # Nearest-rank percentile: no interpolation between neighbouring samples.
    return samples[min(len(samples) - 1, max(0, round(p / 100 * len(samples) + 0.5) - 1))]


print(f"samples          {len(samples)}")
print(f"min       {samples[0]:8.3f} ms")
print(f"p50       {pct(50):8.3f} ms")
print(f"p90       {pct(90):8.3f} ms")
print(f"p95       {pct(95):8.3f} ms")
print(f"p99       {pct(99):8.3f} ms")
print(f"max       {samples[-1]:8.3f} ms")
print(f"budget    {16.7:8.3f} ms (one 60Hz frame)")
print("VERDICT", "PASS" if pct(95) <= 16.7 else "OVER BUDGET")
PY
