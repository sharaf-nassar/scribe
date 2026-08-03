#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted + visual E2E: the three zoom chords in the running GPUI client.
#
# `zoom.rs` is pure arithmetic over an integer point delta, so a `#[gpui::test]`
# over it proves nothing about the running binary — the 016 reachability audit
# recorded `zoom_in` / `zoom_out` / `zoom_reset` as UNWIRED precisely because
# the math was green while the dispatch site swallowed the actions. Every
# assertion here therefore comes from the real window or from the real wire:
#
#   * the grid repaints when the font rescales (a screenshot diff), and
#     `zoom_reset` restores the pre-zoom frame pixel for pixel;
#   * each zoom step re-publishes the pane's geometry, which the wire tap
#     records as a `Resize` carrying a new cell box — smaller cells and more
#     columns when zooming out, the reverse when zooming in, and exactly the
#     pre-zoom geometry again after a reset.
#
# The window is driven through XTEST (`xdotool key`, no `--window`): GPUI reads
# the keyboard through XInput2 and ignores the synthetic XSendEvent input that
# `xdotool --window` delivers, so window-targeted input would leave the client
# untouched while the script still "passed".
#
# Requires the shared-pane rig plus the wire tap:
#   just e2e-visual-terminal-zoom
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the shared-pane rig must export a created SESSION}"

# Lit pixels the grid must hold before any phase runs, measured the way
# terminal-viewport.sh measures it: an unattached window reads a few hundred, a
# live pane showing a prompt reads thousands.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Differing pixels a font rescale must produce inside the grid. Every glyph box
# moves, so the real number is tens of thousands; a swallowed chord yields
# exactly 0 (the image pins SCRIBE_DISABLE_ANIMATIONS=1, so consecutive frames
# of an idle grid are byte-identical).
ZOOM_DIFF_MIN="${ZOOM_DIFF_MIN:-5000}"

# Differing pixels the reset frame may still carry against the pre-zoom frame.
# Non-zero only so a stray antialiased pixel or a late PTY repaint cannot fail
# the restore check; three orders of magnitude below ZOOM_DIFF_MIN.
ZOOM_RESTORE_DIFF_MAX="${ZOOM_RESTORE_DIFF_MAX:-400}"

# Bands excluded from `grid_diff`: the integrated tab strip on top and the
# status line plus the system-stats bar underneath, all of which repaint on
# timers of their own.
GRID_TOP_INSET_PX="${GRID_TOP_INSET_PX:-40}"
GRID_BOTTOM_INSET_PX="${GRID_BOTTOM_INSET_PX:-80}"

POLL_TICKS="${POLL_TICKS:-20}"

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
    scrot -o /output/zoom-fullscreen.png
    convert /output/zoom-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Differing pixels inside the terminal grid only. The status bar's sparklines
# resample on a timer, so a whole-window diff carries noise that would swamp
# the restore assertion.
grid_diff() {
    local h value
    h=$(( WIN_H - GRID_TOP_INSET_PX - GRID_BOTTOM_INSET_PX ))
    value=$(compare -metric AE \
        \( "$1" -crop "${WIN_W}x${h}+0+${GRID_TOP_INSET_PX}" +repage \) \
        \( "$2" -crop "${WIN_W}x${h}+0+${GRID_TOP_INSET_PX}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# Wait until the client log holds more copies of a pattern than `baseline`.
wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" now started
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# The most recent client log line containing $1, with the tracing formatter's
# ANSI colour codes stripped — they sit between the field name and its value
# (`level\e[0m=\e[0m-1`), so an unstripped line matches no `field=value` test.
last_log_line() {
    grep -F "$1" "$CLIENT_LOG" 2>/dev/null | tail -1 | sed -e 's/\x1b\[[0-9;]*m//g'
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

# Geometry of the newest recorded `Resize` for this session, as
# "cols rows cell_width cell_height". Empty when the client has published none.
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
    print(
        newest.get("cols", 0),
        newest.get("rows", 0),
        newest.get("cell_width", 0),
        newest.get("cell_height", 0),
    )
PY
}

# Wait until another `Resize` for this session lands on the wire.
wait_for_resize() {
    local baseline="$1" timeout_secs="${2:-15}" started
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

# Drive one zoom chord and return only once BOTH halves of the wiring landed:
# the view stepped its zoom level, and the new cell box left on the wire.
# Sets ZOOM_LINE, ZOOM_COLS, ZOOM_ROWS, ZOOM_CELL_W and ZOOM_CELL_H.
zoom_step() {
    local chord="$1" label="$2" log_base resize_base
    log_base=$(count_log "terminal zoom changed")
    resize_base=$(count_resizes)
    send_keys "$chord"
    if ! wait_for_log_growth "terminal zoom changed" "$log_base"; then
        fail "FAIL: $chord never reached $label (still swallowed?)"
    fi
    ZOOM_LINE=$(last_log_line "terminal zoom changed")
    if ! wait_for_resize "$resize_base"; then
        fail "FAIL: $label changed the font but published no Resize: $ZOOM_LINE"
    fi
    read -r ZOOM_COLS ZOOM_ROWS ZOOM_CELL_W ZOOM_CELL_H <<<"$(last_resize)"
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/zoom-00-attached.png
    ink=$(window_ink /output/zoom-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: a screenful of glyphs, and the pre-zoom geometry ─────
# Short lines on purpose: nothing may wrap at any zoom level, so the reflow a
# PTY resize triggers cannot itself change what the grid shows. That is what
# lets the reset frame be compared against this one.
scribe-test send "$SESSION" 'for i in $(seq 1 20); do echo "zoom-row-$i"; done; printf "ZOOM_%s\n" READY\n'
scribe-test wait-output "$SESSION" "ZOOM_READY" --timeout 20000 >/dev/null \
    || fail "PHASE 1 FAIL: the seeded rows never reached the session"
sleep 1.0
capture /output/zoom-01-baseline.png
if ! wait_for_resize 0 20; then
    fail "PHASE 1 FAIL: the client published no pane geometry at all"
fi
read -r BASE_COLS BASE_ROWS BASE_CELL_W BASE_CELL_H <<<"$(last_resize)"
echo "PHASE 1 PASS: baseline geometry on the wire is ${BASE_COLS}x${BASE_ROWS} cells of ${BASE_CELL_W}x${BASE_CELL_H} px"

# ── Phase 2: zoom_out shrinks the cell and widens the grid ────────
zoom_step ctrl+minus zoom_out
case "$ZOOM_LINE" in
    *"level=-1"*) ;;
    *) fail "PHASE 2 FAIL: zoom_out did not step the level: $ZOOM_LINE" ;;
esac
capture /output/zoom-02-out.png
DIFF=$(grid_diff /output/zoom-01-baseline.png /output/zoom-02-out.png)
if [ "${DIFF:-0}" -lt "$ZOOM_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: zoom_out changed $DIFF px (min $ZOOM_DIFF_MIN); the font never rescaled"
fi
if [ "$ZOOM_CELL_H" -ge "$BASE_CELL_H" ]; then
    fail "PHASE 2 FAIL: zoom_out published cell height $ZOOM_CELL_H, not below $BASE_CELL_H"
fi
if [ "$ZOOM_COLS" -le "$BASE_COLS" ]; then
    fail "PHASE 2 FAIL: zoom_out published $ZOOM_COLS cols, not above $BASE_COLS"
fi
OUT_COLS="$ZOOM_COLS"
OUT_CELL_H="$ZOOM_CELL_H"
echo "PHASE 2 PASS: ctrl+- rescaled the grid (+$DIFF px) and put ${ZOOM_COLS}x${ZOOM_ROWS} @ ${ZOOM_CELL_W}x${ZOOM_CELL_H} px on the wire"

# ── Phase 3: zoom_in steps back up and past the configured size ───
# Two steps: the first only undoes the zoom-out, the second proves `zoom_in`
# reaches a level the client has never rendered at in this run.
zoom_step ctrl+equal zoom_in
zoom_step ctrl+equal zoom_in
case "$ZOOM_LINE" in
    *"level=1"*) ;;
    *) fail "PHASE 3 FAIL: zoom_in did not step above the configured size: $ZOOM_LINE" ;;
esac
capture /output/zoom-03-in.png
DIFF=$(grid_diff /output/zoom-02-out.png /output/zoom-03-in.png)
if [ "${DIFF:-0}" -lt "$ZOOM_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: zoom_in changed $DIFF px (min $ZOOM_DIFF_MIN); the font never rescaled"
fi
if [ "$ZOOM_CELL_H" -le "$OUT_CELL_H" ]; then
    fail "PHASE 3 FAIL: zoom_in published cell height $ZOOM_CELL_H, not above $OUT_CELL_H"
fi
if [ "$ZOOM_COLS" -ge "$OUT_COLS" ]; then
    fail "PHASE 3 FAIL: zoom_in published $ZOOM_COLS cols, not below $OUT_COLS"
fi
if [ "$ZOOM_COLS" -ge "$BASE_COLS" ]; then
    fail "PHASE 3 FAIL: zoom_in published $ZOOM_COLS cols, not below the baseline $BASE_COLS"
fi
echo "PHASE 3 PASS: ctrl+= rescaled the grid (+$DIFF px) and put ${ZOOM_COLS}x${ZOOM_ROWS} @ ${ZOOM_CELL_W}x${ZOOM_CELL_H} px on the wire"

# ── Phase 4: zoom_reset restores the configured size exactly ──────
zoom_step ctrl+0 zoom_reset
case "$ZOOM_LINE" in
    *"level=0"*) ;;
    *) fail "PHASE 4 FAIL: zoom_reset did not return to the configured level: $ZOOM_LINE" ;;
esac
if [ "$ZOOM_COLS" != "$BASE_COLS" ] || [ "$ZOOM_ROWS" != "$BASE_ROWS" ] \
    || [ "$ZOOM_CELL_W" != "$BASE_CELL_W" ] || [ "$ZOOM_CELL_H" != "$BASE_CELL_H" ]; then
    fail "PHASE 4 FAIL: zoom_reset published ${ZOOM_COLS}x${ZOOM_ROWS} @ ${ZOOM_CELL_W}x${ZOOM_CELL_H}, not the baseline ${BASE_COLS}x${BASE_ROWS} @ ${BASE_CELL_W}x${BASE_CELL_H}"
fi
capture /output/zoom-04-reset.png
DIFF=$(grid_diff /output/zoom-01-baseline.png /output/zoom-04-reset.png)
if [ "${DIFF:-0}" -gt "$ZOOM_RESTORE_DIFF_MAX" ]; then
    fail "PHASE 4 FAIL: the reset grid differs from the pre-zoom grid by $DIFF px (max $ZOOM_RESTORE_DIFF_MAX)"
fi
echo "PHASE 4 PASS: ctrl+0 restored the pre-zoom grid ($DIFF px apart) and re-published ${ZOOM_COLS}x${ZOOM_ROWS} @ ${ZOOM_CELL_W}x${ZOOM_CELL_H} px"

# ── Phase 5: no zoom action was dropped along the way ─────────────
if grep -E "action not wired into the GPUI shell.*Zoom" "$CLIENT_LOG" >/dev/null 2>&1; then
    fail "PHASE 5 FAIL: a Zoom action was still dropped by the shell"
fi
echo "PHASE 5 PASS: no Zoom action reached the unroutable path"

echo ""
echo "ALL PHASES PASS — zoom is reachable, visible, and on the wire."
echo "  Captures:    test-output/zoom-0*.png"
echo "  Wire record: test-output/share-wire.jsonl"
