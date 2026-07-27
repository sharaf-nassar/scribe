#!/bin/bash
# Scripted + visual E2E: a live window resize republishes every pane's grid.
#
# Resizing the window re-lays the panes locally from the grid band's measured
# rect, but that rect is written during *prepaint* — after the `render` that
# reacted to the resize already compared the published area against the stale
# one. With nothing scheduling a follow-up frame, the client used to re-lay the
# panes on screen and never tell the server, so every PTY kept its pre-resize
# size and applications inside them wrapped at the old column count.
#
# A unit test cannot see that: the arithmetic was always right and the missing
# piece was the frame that never happened. Every assertion here therefore comes
# from the real window or the real wire:
#
#   * the grid repaints when the window is resized (a screenshot diff);
#   * the wire tap records a fresh `Resize` carrying MORE columns and rows when
#     the window grows, and fewer when it shrinks back;
#   * the PTY itself agrees — `stty size` inside the session reports exactly the
#     cell counts the client put on the wire, which is only true if the server
#     applied the resize;
#   * and the republish converges: an idle window after the resize settles
#     produces no further `Resize` frames, so the deferred follow-up cannot
#     become a per-frame storm.
#
# The window is resized through the window manager (`xdotool windowsize`), the
# same path a user's drag takes, rather than through any client action.
#
# Requires the shared-pane rig plus the wire tap:
#   just e2e-visual-window-resize
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the shared-pane rig must export a created SESSION}"

# Lit pixels the grid must hold before any phase runs, measured the way
# terminal-zoom.sh measures it: an unattached window reads a few hundred, a live
# pane showing a prompt reads thousands.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# The two window shapes the test drives. Both fit the 1920x1080 Xvfb screen, and
# both differ from the startup size (120x36 cells) by far more than the one cell
# a rounding wobble could account for.
GROWN_W="${GROWN_W:-1700}"
GROWN_H="${GROWN_H:-1000}"
SHRUNK_W="${SHRUNK_W:-900}"
SHRUNK_H="${SHRUNK_H:-600}"

# Differing pixels a resize must produce inside the grid. Every pane rect moves,
# so the real number is tens of thousands; a window that never re-laid yields
# almost none (the image pins SCRIBE_DISABLE_ANIMATIONS=1, so consecutive frames
# of an idle grid are byte-identical).
RESIZE_DIFF_MIN="${RESIZE_DIFF_MIN:-5000}"

POLL_TICKS="${POLL_TICKS:-20}"

# Seconds an idle window is watched for a `Resize` storm once the geometry has
# settled. The deferred republish is armed only by a rect that actually moved,
# so a settled window must produce exactly zero.
IDLE_WATCH_SECS="${IDLE_WATCH_SECS:-4}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -20 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "FAIL: no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.5
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Capture the client window only. A full-screen scrot also catches openbox's
# title bar, whose pixels belong to no phase here.
capture() {
    focus
    sleep 0.4
    scrot -o /output/resize-fullscreen.png
    convert /output/resize-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Differing pixels between two captures over the region they share. The frames
# compared here have different dimensions on purpose (that is the resize), so
# the diff is taken over the smaller frame's box anchored at the top-left.
frame_diff() {
    local w h value
    w=$(convert "$1" -format "%w" info:)
    h=$(convert "$1" -format "%h" info:)
    local w2 h2
    w2=$(convert "$2" -format "%w" info:)
    h2=$(convert "$2" -format "%h" info:)
    [ "$w2" -lt "$w" ] && w="$w2"
    [ "$h2" -lt "$h" ] && h="$h2"
    value=$(compare -metric AE \
        \( "$1" -crop "${w}x${h}+0+0" +repage \) \
        \( "$2" -crop "${w}x${h}+0+0" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

# Recorded client→server `Resize` frames for this session.
count_resizes() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
total = 0
try:
    handle = open(path)
except OSError:
    print(0)
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "Resize":
            continue
        if str(message.get("session_id")) != session:
            continue
        total += 1
print(total)
PY
}

# Geometry of the newest recorded `Resize` for this session, as "cols rows".
# Empty when the client has published none.
last_resize() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
newest = None
try:
    handle = open(path)
except OSError:
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "Resize":
            continue
        if str(message.get("session_id")) != session:
            continue
        newest = message.get("size", {})
if newest is not None:
    print(newest.get("cols", 0), newest.get("rows", 0))
PY
}

# Wait until another `Resize` for this session lands on the wire.
wait_for_resize() {
    local baseline="$1" timeout_secs="${2:-20}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_resizes)" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Ask the PTY what size the server gave it. `stty` is answered by the kernel's
# window size, so a match proves the `Resize` was applied end to end and not
# merely written to a socket.
assert_pty_size() {
    local marker="$1" rows="$2" cols="$3"
    # Exactly one `\n` in the payload: `scribe-test send` translates escapes, so
    # a second one would break the command line in half before the shell ran it.
    scribe-test send "$SESSION" "echo ${marker}=\$(stty size | tr ' ' 'x')\n"
    scribe-test wait-output "$SESSION" "${marker}=${rows}x${cols}" --timeout 15000 >/dev/null \
        || return 1
    return 0
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/resize-00-attached.png
    ink=$(window_ink /output/resize-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: the startup geometry is on the wire and in the PTY ───
if ! wait_for_resize 0 25; then
    fail "PHASE 1 FAIL: the client published no pane geometry at all"
fi
sleep 1.0
read -r BASE_COLS BASE_ROWS <<<"$(last_resize)"
capture /output/resize-01-baseline.png
assert_pty_size PTYBASE "$BASE_ROWS" "$BASE_COLS" \
    || fail "PHASE 1 FAIL: the PTY does not report the published ${BASE_COLS}x${BASE_ROWS}"
echo "PHASE 1 PASS: startup geometry is ${BASE_COLS}x${BASE_ROWS} on the wire and in the PTY"

# ── Phase 2: growing the window republishes a bigger grid ─────────
RESIZE_BASE=$(count_resizes)
WID=$(find_window)
xdotool windowsize "$WID" "$GROWN_W" "$GROWN_H"
if ! wait_for_resize "$RESIZE_BASE" 25; then
    fail "PHASE 2 FAIL: the window grew to ${GROWN_W}x${GROWN_H} but no Resize reached the wire"
fi
sleep 1.0
read -r GROWN_COLS GROWN_ROWS <<<"$(last_resize)"
capture /output/resize-02-grown.png
DIFF=$(frame_diff /output/resize-01-baseline.png /output/resize-02-grown.png)
if [ "${DIFF:-0}" -lt "$RESIZE_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: the grown window differs by only $DIFF px (min $RESIZE_DIFF_MIN)"
fi
if [ "$GROWN_COLS" -le "$BASE_COLS" ] || [ "$GROWN_ROWS" -le "$BASE_ROWS" ]; then
    fail "PHASE 2 FAIL: growing the window published ${GROWN_COLS}x${GROWN_ROWS}, not above ${BASE_COLS}x${BASE_ROWS}"
fi
assert_pty_size PTYGROWN "$GROWN_ROWS" "$GROWN_COLS" \
    || fail "PHASE 2 FAIL: the PTY kept its old size instead of ${GROWN_COLS}x${GROWN_ROWS}"
echo "PHASE 2 PASS: the grown window re-laid the grid (+$DIFF px) and published ${GROWN_COLS}x${GROWN_ROWS} to the PTY"

# ── Phase 3: shrinking republishes a smaller grid ─────────────────
RESIZE_BASE=$(count_resizes)
xdotool windowsize "$WID" "$SHRUNK_W" "$SHRUNK_H"
if ! wait_for_resize "$RESIZE_BASE" 25; then
    fail "PHASE 3 FAIL: the window shrank to ${SHRUNK_W}x${SHRUNK_H} but no Resize reached the wire"
fi
sleep 1.0
read -r SHRUNK_COLS SHRUNK_ROWS <<<"$(last_resize)"
capture /output/resize-03-shrunk.png
if [ "$SHRUNK_COLS" -ge "$GROWN_COLS" ] || [ "$SHRUNK_ROWS" -ge "$GROWN_ROWS" ]; then
    fail "PHASE 3 FAIL: shrinking published ${SHRUNK_COLS}x${SHRUNK_ROWS}, not below ${GROWN_COLS}x${GROWN_ROWS}"
fi
assert_pty_size PTYSHRUNK "$SHRUNK_ROWS" "$SHRUNK_COLS" \
    || fail "PHASE 3 FAIL: the PTY kept ${GROWN_COLS}x${GROWN_ROWS} instead of ${SHRUNK_COLS}x${SHRUNK_ROWS}"
echo "PHASE 3 PASS: the shrunk window published ${SHRUNK_COLS}x${SHRUNK_ROWS} to the PTY"

# ── Phase 4: a settled window publishes nothing further ───────────
SETTLED=$(count_resizes)
sleep "$IDLE_WATCH_SECS"
AFTER=$(count_resizes)
if [ "$AFTER" -ne "$SETTLED" ]; then
    fail "PHASE 4 FAIL: an idle window published $(( AFTER - SETTLED )) more Resize frames in ${IDLE_WATCH_SECS}s"
fi
echo "PHASE 4 PASS: the republish converged — no Resize frames from a settled window"

echo ""
echo "ALL PHASES PASS — a live window resize reaches the server and the PTY."
echo "  Captures:    test-output/resize-0*.png"
echo "  Wire record: test-output/share-wire.jsonl"
