#!/bin/bash
# Scripted E2E: command-palette and context-menu entries actually run.
#
# Both overlays used to open and then do nothing — the shell subscribed to
# `CommandPaletteEvent::Execute` and `ContextMenuEvent::Selected` only to drop
# the payload. This test drives the real window and asserts the *effect* of a
# chosen row, not that the overlay closed. One row per action class:
#
#   * a context-menu row that sends text reaches the attached pane, asserted by
#     the echoed command appearing in the rendered grid;
#   * a palette row that creates a tab produces a real server round trip,
#     asserted through the client's own "opened a new tab" line, which the
#     server only ever answers with after `CreateSession`;
#   * a palette row whose destination surface is not ported yet still reaches
#     the shared dispatcher, asserted through the named "action not wired"
#     warning rather than a silent drop.
#
# Phase 0 exists because the harness cannot otherwise hand the client a pane.
# The entrypoint creates $SESSION through `scribe-test` *after* launching the
# client, and the server sends `SessionCreated` only to the connection that
# asked for it, so the running client never learns the session exists. Worse,
# `handle_list_sessions` hides sessions owned by another window, so even a
# relaunch sees nothing. Stopping the test daemon releases that ownership,
# after which a relaunched client picks the session up through the normal
# `ListSessions` path and attaches to it. The cost is that `scribe-test` can no
# longer observe the session either (`wait-output` needs its daemon), which is
# why the pane assertions below read pixels instead of server-side output.
#
# Input is driven through XTEST (plain `xdotool key` / `click`, no `--window`).
# GPUI reads pointer and keyboard through XInput2 and ignores the synthetic
# events that `xdotool --window` sends with XSendEvent, so window-targeted
# input would leave the client untouched while the script still "passed".
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

# Right-click point, in window-relative pixels.
MENU_CLICK_X=300
MENU_CLICK_Y=300

# Offset of the last context-menu row ("Send Text: …") from the click point.
# The box is anchored at the cursor with a 4px pad; above the last row sit the
# three copy-head rows, one divider, "Open URL", and "Copy hyperlink address".
# Calibrated against 02-context-menu-open.png — a menu layout change turns into
# a failing phase here rather than a silent miss.
MENU_ROW_SEND_DY=184
MENU_ROW_DX=60

# Extra lit pixels the echoed command must add to the grid. The row types
# "scribe-context-menu" and the shell answers "command not found", together far
# more than this; an unrouted click leaves the grid byte-identical.
INK_DELTA_MIN="${INK_DELTA_MIN:-200}"

WIN_X=0
WIN_Y=0
GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

# Focus the client and cache its on-screen geometry, so window-relative
# coordinates can be replayed as absolute XTEST pointer moves and the grid can
# be cropped out of a full-screen capture.
focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found"
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    # Measure the whole client window rather than a guessed grid inset: the
    # first prompt row sits only a few pixels under the tab strip, so a crop
    # tuned to "just the grid" silently drops the very content being measured.
    # Nothing else in the window changes between the two captures that bracket
    # the context-menu click.
    GRID_X="$X"
    GRID_Y="$Y"
    GRID_W="$WIDTH"
    GRID_H="$HEIGHT"
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Count lit pixels in the client window of a full-screen capture. Rendered text
# is near-white on a near-black background, so a plain luminance threshold
# separates ink from the pane cleanly.
grid_ink() {
    local value
    value=$(convert "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.4
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.4
}

# Click at a window-relative point with the given button, through XTEST.
click_at() {
    local x="$1" y="$2" button="$3"
    xdotool mousemove "$(( WIN_X + x ))" "$(( WIN_Y + y ))"
    sleep 0.3
    xdotool click "$button"
    sleep 0.5
}

# Count matching lines in the client log (0 when the log does not exist yet).
count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# Wait until the client log holds more copies of a pattern than `baseline`.
wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started now
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

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    exit 1
}

# ── Phase 0: hand the client a live pane to act in ────────────────
# The old window must be gone before the new one starts: two mapped Scribe
# windows and every capture below could be of the wrong one, which would read
# as a routing failure.
sleep 1.0
kill "$SCRIBE_CLIENT_PID" 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f 'scribe-client-gpui' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client-gpui' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not exit"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
scribe-client-gpui >>"$CLIENT_LOG" 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2

# Attaching is a full Hello / ListSessions / AttachSessions / SessionReplay
# round trip, so poll for the prompt to render rather than guessing a delay.
BASE_INK=0
for _ in $(seq 1 40); do
    focus
    shot /output/00-attached.png >/dev/null
    BASE_INK=$(grid_ink /output/00-attached.png)
    [ "$BASE_INK" -ge 20 ] && break
    sleep 0.5
done
if [ "$BASE_INK" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (ink $BASE_INK)"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (grid ink $BASE_INK)"

# ── Phase 1: the right-click menu opens over the grid ─────────────
click_at "$MENU_CLICK_X" "$MENU_CLICK_Y" 3
shot /output/01-context-menu-open.png
echo "PHASE 1 PASS: right-click opened the context menu at the cursor"

# ── Phase 2: a context-menu row reaches the attached pane ─────────
# The clicked row sends text to the focused pane. The shell echoes it and
# answers, so a routed click shows up as new ink in the grid; a dropped one
# leaves the pane exactly as phase 0 captured it.
click_at "$(( MENU_CLICK_X + MENU_ROW_DX ))" \
    "$(( MENU_CLICK_Y + MENU_ROW_SEND_DY ))" 1
sleep 1.0
shot /output/02-context-menu-dispatched.png
AFTER_INK=$(grid_ink /output/02-context-menu-dispatched.png)
DELTA=$(( AFTER_INK - BASE_INK ))
if [ "$DELTA" -lt "$INK_DELTA_MIN" ]; then
    fail "PHASE 2 FAIL: the clicked row changed the pane by $DELTA px (min $INK_DELTA_MIN)"
fi
echo "PHASE 2 PASS: the clicked context-menu row typed into the attached pane (+$DELTA px)"

# ── Phase 3: a palette row creates a real session ─────────────────
# Confirming "New Tab" has to reach `CreateSession` on the wire and come back
# as `SessionCreated`; nothing short of a live round trip produces this line.
TABS_BEFORE=$(count_log "opened a new tab")
focus
send_keys ctrl+shift+p
type_text "New Tab"
shot /output/03-palette-new-tab.png
send_keys Return
if ! wait_for_log_growth "opened a new tab" "$TABS_BEFORE" 20; then
    fail "PHASE 3 FAIL: confirming the 'New Tab' row created no session"
fi
shot /output/04-palette-tab-created.png
echo "PHASE 3 PASS: the palette 'New Tab' row round-tripped a real session"

# ── Phase 4: an unported row still reaches the dispatcher ─────────
# "Open Settings" has no destination surface in this client yet. The point of
# this phase is that the row is *routed*: it lands on the shared dispatcher and
# is named and counted there, instead of being discarded at the subscription
# before any handler sees it.
UNWIRED_BEFORE=$(count_log "action not wired into the GPUI shell")
focus
send_keys ctrl+shift+p
type_text "Open Settings"
send_keys Return
if ! wait_for_log_growth "action not wired into the GPUI shell" "$UNWIRED_BEFORE" 10; then
    fail "PHASE 4 FAIL: the 'Open Settings' row was dropped before the dispatcher"
fi
shot /output/05-palette-unported-row.png
echo "PHASE 4 PASS: an unported palette row reaches the shared dispatcher"

echo ""
echo "PASS: visual overlay-actions test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png                — the adopted pane before any action"
echo "    01-context-menu-open.png       — menu open at the cursor"
echo "    02-context-menu-dispatched.png — the sent text echoed in the pane"
echo "    03-palette-new-tab.png         — palette filtered to 'New Tab'"
echo "    04-palette-tab-created.png     — the new tab in the strip"
echo "    05-palette-unported-row.png    — palette after an unported row"
