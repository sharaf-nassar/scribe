#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: command-palette and context-menu entries actually run.
#
# Both overlays used to open and then do nothing — the shell subscribed to
# `CommandPaletteEvent::Execute` and `ContextMenuEvent::Selected` only to drop
# the payload. This test drives the real window and asserts the *effect* of a
# chosen row, not that the overlay closed. One row per action class:
#
#   * a context-menu row that sends text reaches the attached pane, asserted on
#     the session's PTY through `scribe-test` and again as new ink in the
#     rendered grid;
#   * a palette row that creates a tab produces a real server round trip,
#     asserted through the client's own "opened a new tab" line, which the
#     server only ever answers with after `CreateSession`;
#   * a palette row whose destination is another top-level window ("Open
#     Settings") reaches the shared dispatcher and puts that window on screen,
#     asserted through the client's own line and the mapped X11 window.
#
# The client is handed a live pane by the shared-pane rig (SCRIBE_SHARED_PANE=1,
# see docker/entrypoint-visual.sh): `scribe-test` creates the session and the
# client joins that same window's share additively, so both stay attached. This
# script used to open with a phase 0 that killed the client, stopped the test
# daemon to release window ownership, and relaunched — the only way to get a
# pane in front of the camera before the rig existed, and one that cost every
# server-side assertion, since `wait-output` needs the daemon that had just been
# stopped. It is gone: phase 2 now confirms the routed keystroke through BOTH
# the rendered grid and `scribe-test wait-output` on the same session.
#
# Input is driven through XTEST (plain `xdotool key` / `click`, no `--window`).
# GPUI reads pointer and keyboard through XInput2 and ignores the synthetic
# events that `xdotool --window` sends with XSendEvent, so window-targeted
# input would leave the client untouched while the script still "passed".
#
# Requires: visual container run with SCRIBE_SHARED_PANE=1
# (`just e2e-visual-shared visual/overlay-actions.sh`), which exports SESSION,
# SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
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

# The exact text the demo "Send Text: …" row types into the focused pane
# (DEMO_SMART_ACTION_TEXT in crates/scribe-client/src/main.rs). Asserted
# on the PTY through `scribe-test`, which the shared-pane rig keeps attached to
# the same session the client renders.
SEND_TEXT_ROW_PAYLOAD="scribe-context-menu"

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

# ── Phase 0: the rig's shared pane is on screen ───────────────────
# The entrypoint already gated on the client's attach line, so this only has to
# confirm that the attached pane is actually painted before any action is
# driven at it — an empty grid would make every later ink delta meaningless.
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
#
# Both halves are asserted. The PTY assertion is the precise one — it names the
# exact bytes the row is supposed to have sent, through the daemon that the
# shared-pane rig keeps attached alongside the client — and the ink delta is
# what proves those bytes also reached the window on screen rather than only the
# server.
click_at "$(( MENU_CLICK_X + MENU_ROW_DX ))" \
    "$(( MENU_CLICK_Y + MENU_ROW_SEND_DY ))" 1
if ! scribe-test wait-output "$SESSION" "$SEND_TEXT_ROW_PAYLOAD" >/dev/null 2>&1; then
    fail "PHASE 2 FAIL: '$SEND_TEXT_ROW_PAYLOAD' never reached the session's PTY"
fi
sleep 1.0
shot /output/02-context-menu-dispatched.png
AFTER_INK=$(grid_ink /output/02-context-menu-dispatched.png)
DELTA=$(( AFTER_INK - BASE_INK ))
if [ "$DELTA" -lt "$INK_DELTA_MIN" ]; then
    fail "PHASE 2 FAIL: the clicked row changed the pane by $DELTA px (min $INK_DELTA_MIN)"
fi
echo "PHASE 2 PASS: the clicked row typed into the attached pane (PTY echo + $DELTA px)"

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

# ── Phase 4: a row whose surface is another window ────────────────
# "Open Settings" does not act on the grid at all: it lowers onto
# `KeyAction::OpenSettings` and opens a second top-level window. It is kept here
# as the palette's non-terminal row — proof that a routed row reaches a handler
# that lives outside this window — while `visual/settings-entry.sh` owns the
# full entry-point matrix (chord, palette, gear, and the no-duplicate rule).
SETTINGS_BEFORE=$(count_log "opened the settings window")
focus
send_keys ctrl+shift+p
type_text "Open Settings"
send_keys Return
if ! wait_for_log_growth "opened the settings window" "$SETTINGS_BEFORE" 15; then
    fail "PHASE 4 FAIL: the 'Open Settings' row never opened the settings window"
fi
if ! xdotool search --name '^Scribe Settings$' >/dev/null 2>&1; then
    fail "PHASE 4 FAIL: no settings window mapped after the 'Open Settings' row"
fi
shot /output/05-palette-settings-row.png
echo "PHASE 4 PASS: the 'Open Settings' row opened the settings window"

echo ""
echo "PASS: visual overlay-actions test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png                — the shared pane before any action"
echo "    01-context-menu-open.png       — menu open at the cursor"
echo "    02-context-menu-dispatched.png — the sent text echoed in the pane"
echo "    03-palette-new-tab.png         — palette filtered to 'New Tab'"
echo "    04-palette-tab-created.png     — the new tab in the strip"
echo "    05-palette-settings-row.png    — the settings window the row opened"
