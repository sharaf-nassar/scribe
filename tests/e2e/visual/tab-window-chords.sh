#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the Tabs-and-windows chords reach their actions.
#
# Two parity rows were unreachable for the whole rebuild because the shell
# hard-coded overlay chords on top of the Linux defaults: Ctrl+Shift+Q
# (`close_tab`) opened the close-dialog demo, and Ctrl+Shift+N (`new_window`)
# opened the since-removed notes modal. Both keystrokes were claimed by
# `handle_overlay_key` and never reached `handle_binding`, so the actions could
# only be run from the command palette or after a rebind.
#
# A headless test can prove the *precedence rule* (it does — see the
# keybindings suite), but only the running client can prove that the chord a
# user actually presses now lands on the action. So each phase drives the real
# window through XTEST and asserts an effect the action alone can produce:
#
#   * Ctrl+Shift+Q makes the client log "closing the active tab" — a line only
#     `close_active_tab` writes and only `LayoutAction::CloseTab` reaches — and
#     the server answer it, logging "session closed by client";
#   * Ctrl+Shift+N makes the client log "opened a new terminal window", adds a
#     second mapped X11 window, and makes the server register a second window
#     through a fresh `Hello`; a dialog or a swallowed chord does none of it;
#   * Ctrl+Shift+D still opens the close dialog on its relocated chord, so
#     moving the overlay off `close_tab`'s default did not strand the surface.
#
# Phase 0 is the same session-adoption dance `overlay-actions.sh` documents:
# the entrypoint creates $SESSION after the client launched, so the running
# client never hears about it, and only a relaunch after the test daemon
# releases ownership picks it up through `ListSessions`.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI
# reads the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent, so window-targeted input would
# leave the client untouched while the script still "passed".
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Differing pixels a modal must add to the frame. The dialog dims the whole
# backdrop and draws a box over the grid, which is far more than this; a
# swallowed chord leaves consecutive frames byte-identical (animations are off).
DIALOG_DIFF_MIN="${DIALOG_DIFF_MIN:-20000}"

GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

# Every mapped Scribe window, newest last.
list_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

count_windows() {
    list_windows | grep -c . || true
}

find_window() {
    list_windows | tail -1
}

# Focus the client and cache its on-screen geometry so a full-screen capture
# can be cropped down to the window under test.
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

# Count differing pixels between two captures, cropped to the client window.
window_diff() {
    local value
    value=$(compare -metric AE \
        \( "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        \( "$2" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.4
}

# Count matching lines in a log (0 when the log does not exist yet).
count_in() {
    grep -acF "$2" "$1" 2>/dev/null || true
}

count_log() {
    count_in "$CLIENT_LOG" "$1"
}

count_server_log() {
    count_in "$SERVER_LOG" "$1"
}

# Wait until a log holds more copies of a pattern than `baseline`.
wait_for_log_growth_in() {
    local file="$1" pattern="$2" baseline="$3" timeout_secs="${4:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_in "$file" "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

wait_for_log_growth() {
    wait_for_log_growth_in "$CLIENT_LOG" "$1" "$2" "${3:-15}"
}

wait_for_server_log_growth() {
    wait_for_log_growth_in "$SERVER_LOG" "$1" "$2" "${3:-15}"
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" || true
    exit 1
}

# ── Phase 0: hand the client a live pane to act in ────────────────
sleep 1.0
# CLOSED, not killed. A killed client leaves its own login shell running in a
# window the server keeps, and the relaunch below reopens every window the
# server still holds sessions for — so a kill here would leave two windows and
# break the count this phase establishes. "Kill Window" (ctrl+shift+d, then Tab
# twice off the safe Cancel default) destroys it on the server instead.
focus
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
for _ in $(seq 1 80); do
    pgrep -f 'scribe-client' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not close"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
scribe-client >>"$CLIENT_LOG" 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2

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
WINDOWS_BEFORE=$(count_windows)
if [ "$WINDOWS_BEFORE" -ne 1 ]; then
    fail "PHASE 0 FAIL: expected exactly one Scribe window, found $WINDOWS_BEFORE"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (grid ink $BASE_INK, 1 window)"

# ── Phase 1: Ctrl+Shift+Q reaches close_tab ───────────────────────
# The close dialog used to own this chord. "closing the active tab" is written
# only by `close_active_tab`, which only `LayoutAction::CloseTab` reaches, so
# the line appearing means the chord got past the overlay layer.
CLOSES_BEFORE=$(count_log "closing the active tab")
SERVER_CLOSES_BEFORE=$(count_server_log "session closed by client")
focus
send_keys ctrl+shift+q
if ! wait_for_log_growth "closing the active tab" "$CLOSES_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+shift+q never reached close_tab (still shadowed?)"
fi
# The client's own line proves the chord dispatched; the server's proves the
# CloseSession it sent actually crossed the wire and killed the session.
if ! wait_for_server_log_growth "session closed by client" "$SERVER_CLOSES_BEFORE" 15; then
    fail "PHASE 1 FAIL: the server never saw a CloseSession for the active tab"
fi
shot /output/01-close-tab.png
echo "PHASE 1 PASS: ctrl+shift+q closed the tab end to end (client chord, server close)"

# ── Phase 2: Ctrl+Shift+N opens a second window ───────────────────
# A since-removed modal used to own this chord, and `NewWindow` had no
# handler at all. Both the log line and a second mapped X11 window are asserted:
# the log proves the action ran, the window count proves it really opened one.
WINDOWS_LOG_BEFORE=$(count_log "opened a new terminal window")
HELLOS_BEFORE=$(count_server_log "client identified via Hello")
focus
send_keys ctrl+shift+n
if ! wait_for_log_growth "opened a new terminal window" "$WINDOWS_LOG_BEFORE" 15; then
    fail "PHASE 2 FAIL: ctrl+shift+n never reached new_window (still shadowed?)"
fi
# The second window is a separate client to the server: its own connection, its
# own Hello, and therefore its own window id and sessions.
if ! wait_for_server_log_growth "client identified via Hello" "$HELLOS_BEFORE" 15; then
    fail "PHASE 2 FAIL: the new window opened no connection of its own"
fi
WINDOWS_AFTER=0
for _ in $(seq 1 40); do
    WINDOWS_AFTER=$(count_windows)
    [ "$WINDOWS_AFTER" -gt "$WINDOWS_BEFORE" ] && break
    sleep 0.5
done
if [ "$WINDOWS_AFTER" -le "$WINDOWS_BEFORE" ]; then
    fail "PHASE 2 FAIL: new_window mapped no second window (still $WINDOWS_AFTER)"
fi
shot /output/02-new-window.png
echo "PHASE 2 PASS: ctrl+shift+n opened a second window ($WINDOWS_BEFORE -> $WINDOWS_AFTER)"

# ── Phase 3: the close dialog survives on its new chord ───────────
# Relocating the overlay off close_tab's default must not strand it, so the
# dialog is opened on ctrl+shift+d and asserted as a real repaint of the
# window it opens over.
focus
shot /output/03-before-dialog.png
send_keys ctrl+shift+d
shot /output/04-close-dialog.png
DIFF=$(window_diff /output/03-before-dialog.png /output/04-close-dialog.png)
if [ "${DIFF:-0}" -lt "$DIALOG_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: ctrl+shift+d changed $DIFF px (min $DIALOG_DIFF_MIN); dialog did not open"
fi
send_keys Escape
echo "PHASE 3 PASS: the close dialog opens on its relocated chord (+$DIFF px)"

echo ""
echo "PASS: visual tab-window-chords test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png      — the adopted pane before any chord"
echo "    01-close-tab.png     — after ctrl+shift+q closed the tab"
echo "    02-new-window.png    — the second window ctrl+shift+n opened"
echo "    03-before-dialog.png — the window before the dialog chord"
echo "    04-close-dialog.png  — the close dialog on ctrl+shift+d"
