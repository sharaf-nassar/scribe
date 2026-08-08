#!/bin/bash
# @lat: [[test#Visual E2E Tests#Published columns fit one rendered row]]
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Visual E2E: the columns the client publishes must be the columns it paints.
#
# The client owns no PTY. It measures its grid band, divides by the cell box,
# and tells the server "this pane is N columns wide"; the server sets that on
# the PTY and every application right-pads to it. If the client's arithmetic
# reserves even one column more than the pane can paint, the last cell of every
# full-width line lands outside the pane's `overflow_hidden` box, the terminal
# wraps it, and output like pre-commit's status column breaks across two rows
# ("Passed" rendered as "Pa" + "ssed"). Nothing in the suite checked that the
# published width and the painted width are the same number.
#
# Oracle: ink ROWS, measured from a screenshot, compared against themselves.
# A line of exactly N characters must occupy one row; a line of N+1 must occupy
# two. Running both in one pass with one threshold makes the check
# self-calibrating — there is no absolute pixel budget to tune, and the N+1 case
# is the positive control proving the measurement can see a wrap at all.
#
# Phases:
#   0. read the cols the client published, and confirm the PTY agrees;
#   1. N-1 and N characters each paint exactly one row;
#   2. N+1 characters paint two — the wrap detector works.
#
# Requires: the shared-pane visual rig, which exports SESSION —
#   just e2e-visual-shared visual/pane-grid-width.sh
#
# UNVERIFIED: written alongside the fix for scribe-6uk but never executed; the
# harness was held by another run. Treat a first failure as a script bug until
# the phase-2 positive control is seen to pass.

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
POLL_TICKS="${POLL_TICKS:-20}"

# The bottom status bar is lit whether or not a pane is attached, and the
# titlebar carries the tab strip; both would read as ink rows.
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-24}"
TITLE_BAR_INSET_PX="${TITLE_BAR_INSET_PX:-40}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

fail() {
    echo "FAIL: $1" >&2
    echo "--- published grid sizes ---" >&2
    grep -o "published a pane's grid size.*" "$CLIENT_LOG" | tail -10 >&2 || true
    echo "--- screenshots in /output: pane-grid-*.png ---" >&2
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
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    xdotool windowraise "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Full-screen capture cropped to the client's grid band: Vulkan surfaces may not
# be readable per-window, and an uncropped shot catches the WM title bar.
capture_grid() {
    local out="$1" band
    focus
    sleep 0.4
    scrot -o /output/pane-grid-fullscreen.png
    band=$(( WIN_H - STATUS_BAR_INSET_PX - TITLE_BAR_INSET_PX ))
    convert /output/pane-grid-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \
        -crop "${WIN_W}x${band}+0+${TITLE_BAR_INSET_PX}" +repage "$out"
}

# Rows of the cropped band that hold any ink.
#
# Threshold to a 0/1 ink mask FIRST, then collapse each row to its mean and keep
# the rows above a small floor: a single wrapped character is ~0.7% of a row, so
# the floor sits well under that and well over the zero a clean row reads.
ink_rows() {
    local value
    value=$(convert "$1" -colorspace Gray -threshold 35% \
        -resize "1x!" -threshold 0.2% -format "%[fx:mean*h]" info:)
    printf '%s' "${value%.*}"
}

# Paint `$1` copies of '#' on an otherwise clean screen and count the ink rows
# the line itself occupies. The prompt is cleared first and the shell is left at
# a bare prompt afterwards, so the delta between two runs is the line alone.
render_line_rows() {
    local count="$1" tag="$2" rows
    scribe-test send "$SESSION" "clear; printf '%.0s#' \$(seq 1 $count); printf '\\\\n'\n"
    scribe-test wait-idle "$SESSION" --ms 800
    capture_grid "/output/pane-grid-${tag}.png"
    rows=$(ink_rows "/output/pane-grid-${tag}.png")
    printf '%s' "$rows"
}

# ── Phase 0: the published width, and the PTY that got it ─────────
COLS=""
for _ in $(seq 1 "$POLL_TICKS"); do
    COLS=$(grep -o "published a pane's grid size.*cols=[0-9]*" "$CLIENT_LOG" \
        | tail -1 | grep -o 'cols=[0-9]*' | cut -d= -f2)
    [ -n "$COLS" ] && break
    sleep 0.5
done
[ -n "$COLS" ] || fail "PHASE 0: the client never published a pane grid size"
# The fallback grid is COLUMNS (120) cells wide at the nominal metrics and must
# never reach the server; a real measurement lands a column or two under it.
echo "PHASE 0: the client published cols=$COLS"

scribe-test send "$SESSION" 'printf "PTY_COLS=%s.\n" "$(tput cols)"\n'
scribe-test wait-output "$SESSION" "PTY_COLS=${COLS}." \
    || fail "PHASE 0: the client published cols=$COLS but the PTY does not report it"
echo "PHASE 0 PASS: the PTY runs at the published $COLS columns"

# ── Phase 1: N-1 and N characters each fit on one row ─────────────
ROWS_SHORT=$(render_line_rows "$(( COLS - 1 ))" "short")
ROWS_EXACT=$(render_line_rows "$COLS" "exact")
echo "PHASE 1: $(( COLS - 1 )) chars -> $ROWS_SHORT ink row(s); $COLS chars -> $ROWS_EXACT"
[ "$ROWS_SHORT" = "$ROWS_EXACT" ] \
    || fail "PHASE 1: a line of exactly $COLS characters painted $ROWS_EXACT ink rows where $(( COLS - 1 )) painted $ROWS_SHORT — the client publishes more columns than it renders"
echo "PHASE 1 PASS: a $COLS-character line occupies exactly one rendered row"

# ── Phase 2: N+1 characters must wrap (the detector works) ────────
ROWS_OVER=$(render_line_rows "$(( COLS + 1 ))" "over")
echo "PHASE 2: $(( COLS + 1 )) chars -> $ROWS_OVER ink row(s)"
[ "$ROWS_OVER" -gt "$ROWS_EXACT" ] \
    || fail "PHASE 2: a line of $(( COLS + 1 )) characters painted $ROWS_OVER rows, the same as $COLS — the ink-row measurement cannot see a wrap, so phase 1 proved nothing"
echo "PHASE 2 PASS: one character past the published width wraps, as it must"

echo ""
echo "PASS: the published pane width is the painted pane width"
