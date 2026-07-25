#!/bin/bash
# Scripted E2E: the pane and workspace layout is live in the running client.
#
# `pane_tree`, `workspace_tree` and `workspace_layout` shipped with green
# `#[gpui::test]` coverage but were never instantiated by the binary, so all
# fourteen pane/workspace `LayoutAction`s were intercepted no-ops. A headless
# test cannot tell those two states apart — the models pass either way — so
# every phase here drives the real window through XTEST and asserts an effect
# only a live layout can produce:
#
#   * Ctrl+Shift+\ splits the focused pane: the client logs the split, asks the
#     server for a second session, adopts it into the new pane, and republishes
#     BOTH panes at roughly half the window's columns;
#   * typing into the split window puts ink in the RIGHT half of the grid only
#     — the new pane is focused and is a separate terminal, not a second view
#     of the first;
#   * Shift+Ctrl+Alt+Left moves focus back to the left pane, and typing then
#     puts ink in the LEFT half — directional pane focus really moved where
#     keystrokes go;
#   * Ctrl+Tab cycles pane focus (the `cycle_pane` row);
#   * Ctrl+Shift+W closes the focused pane and the server sees its session end;
#   * Ctrl+Alt+\ splits the WINDOW into a second workspace region and
#     Ctrl+Alt+Left moves focus back to the first — the outer layer of the tree,
#     which is a different action family from the pane splits above.
#
# Phase 0 is the same session-adoption dance `tab-window-chords.sh` documents:
# the entrypoint creates $SESSION after the client launched, so the running
# client never hears about it, and only a relaunch after the test daemon
# releases ownership picks it up through `ListSessions`.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI reads
# the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent.
#
# Every chord below is the shipped Linux default except `workspace_focus_left`.
# Its default is Ctrl+Alt+Left, which openbox — the window manager the visual
# container has to run, because the client's X11 active-window guard needs a
# real `_NET_ACTIVE_WINDOW` owner — grabs for "switch to the desktop on the
# left". A grabbed chord never reaches any application, so the rig rebinds that
# one action through SCRIBE_EXTRA_CONFIG (see the run command in
# `just e2e-visual`). That is a property of the harness's WM, not of the client:
# the workspace *split* on the same layer still fires from its untouched
# default, which is what proves the chord path itself is intact.
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG. Run it with:
#   -e SCRIBE_EXTRA_CONFIG=$'[keybindings]\nworkspace_focus_left = "ctrl+alt+h"'
set -e

# The chord `workspace_focus_left` is bound to for this run. Kept in a variable
# so the rebind above and the keypress below can never drift.
WORKSPACE_FOCUS_LEFT_CHORD="${WORKSPACE_FOCUS_LEFT_CHORD:-ctrl+alt+h}"

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Chrome bands that are not part of the pane canvas: the titlebar above it
# (titlebar.rs TITLEBAR_HEIGHT) and the status strip + status bar below it
# (main.rs renders 26 px and 24 px bands). Everything between is the grid area
# the pane layout owns, and is the only region these assertions look at.
TITLEBAR_H=34
BOTTOM_BANDS_H=50

# Ink a typed marker line must add to a half-pane. One `echo` command line plus
# its echoed output is thousands of lit pixels; a pane that never received the
# keystrokes changes by nothing at all (animations are off in the container).
INK_DELTA_MIN="${INK_DELTA_MIN:-150}"

# Differing pixels the split itself must add to the window. A split reflows both
# grids and draws the focus ring, which is far more than this.
SPLIT_DIFF_MIN="${SPLIT_DIFF_MIN:-2000}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

list_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

find_window() {
    list_windows | tail -1
}

# Focus the client and cache its on-screen geometry so a full-screen capture
# can be cropped down to the window, and the window down to its grid area.
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
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

shot() {
    sleep 0.4
    scrot -o "$1"
    echo "captured $1"
}

# Count lit pixels in one horizontal half of the grid area. `$1` is the capture,
# `$2` is "left" or "right". Rendered text is near-white on a near-black
# background, so a plain luminance threshold separates ink from the pane.
half_ink() {
    local file="$1" half="$2"
    local grid_h=$(( WIN_H - TITLEBAR_H - BOTTOM_BANDS_H ))
    local half_w=$(( WIN_W / 2 ))
    local off_x=$WIN_X
    if [ "$half" = "right" ]; then
        off_x=$(( WIN_X + half_w ))
    fi
    local off_y=$(( WIN_Y + TITLEBAR_H ))
    local value
    value=$(convert "$file" -crop "${half_w}x${grid_h}+${off_x}+${off_y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Count differing pixels between two captures, cropped to the client window.
window_diff() {
    local value
    value=$(compare -metric AE \
        \( "$1" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \) \
        \( "$2" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_line() {
    xdotool type --delay 25 "$1"
    xdotool key --clearmodifiers Return
    sleep 1.0
}

count_in() {
    grep -acF "$2" "$1" 2>/dev/null || true
}

# The client log is written by `tracing_subscriber::fmt`, which colours field
# names with SGR escapes, so `field=value` is never contiguous in the raw file.
# Structured assertions run against this stripped view instead.
plain_client_log() {
    sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"
}

count_log() {
    count_in "$CLIENT_LOG" "$1"
}

count_server_log() {
    count_in "$SERVER_LOG" "$1"
}

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
    tail -60 "$CLIENT_LOG" || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" || true
    exit 1
}

# ── Phase 0: hand the client a live pane to act in ────────────────
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

BASE_LEFT=0
for _ in $(seq 1 40); do
    focus
    shot /output/00-single-pane.png >/dev/null
    BASE_LEFT=$(half_ink /output/00-single-pane.png left)
    [ "$BASE_LEFT" -ge 20 ] && break
    sleep 0.5
done
if [ "$BASE_LEFT" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (left-half ink $BASE_LEFT)"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (left-half ink $BASE_LEFT)"

# ── Phase 1: Ctrl+Shift+\ splits the focused pane ─────────────────
# Three independent proofs that a real tree exists: the split log line (only
# `PaneShell::split_focused_pane` reaches it), the second session the new pane
# asks the server for and adopts, and a repaint of the window.
SPLITS_BEFORE=$(count_log "split the focused pane")
ADOPTS_BEFORE=$(count_log "pane adopted a session")
focus
shot /output/01-before-split.png
send_keys ctrl+shift+backslash
if ! wait_for_log_growth "split the focused pane" "$SPLITS_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+shift+backslash never reached split_vertical"
fi
if ! wait_for_log_growth "pane adopted a session" "$ADOPTS_BEFORE" 20; then
    fail "PHASE 1 FAIL: the new pane never got a session from the server"
fi
sleep 1.5
focus
shot /output/02-after-split.png
SPLIT_DIFF=$(window_diff /output/01-before-split.png /output/02-after-split.png)
if [ "${SPLIT_DIFF:-0}" -lt "$SPLIT_DIFF_MIN" ]; then
    fail "PHASE 1 FAIL: the split changed $SPLIT_DIFF px (min $SPLIT_DIFF_MIN); nothing rendered"
fi
# Both panes must have been republished at roughly half the window's columns.
# The window opens 120 columns wide, so a vertical split lands each pane near 60
# and never near 120.
HALVED=$(plain_client_log | grep -cE "published a pane's grid size.*cols=(5[0-9]|6[0-5]) " || true)
if [ "${HALVED:-0}" -lt 2 ]; then
    echo "--- pane size lines ---"
    plain_client_log | grep -F "published a pane's grid size" | tail -10 || true
    fail "PHASE 1 FAIL: $HALVED panes were resized to half the window's columns (want 2)"
fi
echo "PHASE 1 PASS: ctrl+shift+backslash split the pane (+$SPLIT_DIFF px, both panes resized)"

# ── Phase 2: the new pane is focused and is its own terminal ──────
# The split focuses the pane it created, which is the right-hand one, so the
# marker must land on the right and leave the left alone.
focus
LEFT_BEFORE=$(half_ink /output/02-after-split.png left)
RIGHT_BEFORE=$(half_ink /output/02-after-split.png right)
type_line "echo PANE-RIGHT-MARKER"
focus
shot /output/03-typed-right.png
LEFT_AFTER=$(half_ink /output/03-typed-right.png left)
RIGHT_AFTER=$(half_ink /output/03-typed-right.png right)
RIGHT_DELTA=$(( RIGHT_AFTER - RIGHT_BEFORE ))
LEFT_DELTA=$(( LEFT_AFTER - LEFT_BEFORE ))
if [ "$RIGHT_DELTA" -lt "$INK_DELTA_MIN" ]; then
    fail "PHASE 2 FAIL: typing added $RIGHT_DELTA px to the right pane (min $INK_DELTA_MIN)"
fi
if [ "$LEFT_DELTA" -ge "$RIGHT_DELTA" ]; then
    fail "PHASE 2 FAIL: the left pane changed as much as the right ($LEFT_DELTA vs $RIGHT_DELTA)"
fi
echo "PHASE 2 PASS: keystrokes reached the new right pane only (right +$RIGHT_DELTA, left +$LEFT_DELTA)"

# ── Phase 3: directional focus moves where keystrokes go ─────────
FOCUS_BEFORE=$(count_log "focused pane moved")
focus
send_keys shift+ctrl+alt+Left
if ! wait_for_log_growth "focused pane moved" "$FOCUS_BEFORE" 15; then
    fail "PHASE 3 FAIL: shift+ctrl+alt+Left never reached focus_left"
fi
LEFT_BEFORE=$(half_ink /output/03-typed-right.png left)
RIGHT_BEFORE=$(half_ink /output/03-typed-right.png right)
type_line "echo PANE-LEFT-MARKER"
focus
shot /output/04-typed-left.png
LEFT_AFTER=$(half_ink /output/04-typed-left.png left)
RIGHT_AFTER=$(half_ink /output/04-typed-left.png right)
LEFT_DELTA=$(( LEFT_AFTER - LEFT_BEFORE ))
RIGHT_DELTA=$(( RIGHT_AFTER - RIGHT_BEFORE ))
if [ "$LEFT_DELTA" -lt "$INK_DELTA_MIN" ]; then
    fail "PHASE 3 FAIL: after focus_left, typing added $LEFT_DELTA px to the left pane"
fi
if [ "$RIGHT_DELTA" -ge "$LEFT_DELTA" ]; then
    fail "PHASE 3 FAIL: the right pane changed as much as the left ($RIGHT_DELTA vs $LEFT_DELTA)"
fi
echo "PHASE 3 PASS: focus_left moved the keyboard to the left pane (left +$LEFT_DELTA, right +$RIGHT_DELTA)"

# ── Phase 4: Ctrl+Tab cycles pane focus ──────────────────────────
CYCLE_BEFORE=$(count_log "focused pane moved")
focus
send_keys ctrl+Tab
if ! wait_for_log_growth "focused pane moved" "$CYCLE_BEFORE" 15; then
    fail "PHASE 4 FAIL: ctrl+Tab never reached cycle_pane"
fi
echo "PHASE 4 PASS: ctrl+Tab cycled pane focus"

# ── Phase 5: Ctrl+Shift+W closes the focused pane ────────────────
CLOSES_BEFORE=$(count_log "closed the focused pane")
SERVER_CLOSES_BEFORE=$(count_server_log "session closed by client")
focus
send_keys ctrl+shift+w
if ! wait_for_log_growth "closed the focused pane" "$CLOSES_BEFORE" 15; then
    fail "PHASE 5 FAIL: ctrl+shift+w never reached close_pane"
fi
# The client's line proves the layout dropped the pane; the server's proves the
# CloseSession it sent crossed the wire and ended that pane's shell.
if ! wait_for_server_log_growth "session closed by client" "$SERVER_CLOSES_BEFORE" 15; then
    fail "PHASE 5 FAIL: the server never saw a CloseSession for the closed pane"
fi
sleep 1.0
focus
shot /output/05-after-close.png
echo "PHASE 5 PASS: ctrl+shift+w closed the pane end to end (client layout, server session)"

# ── Phase 6: Ctrl+Alt+\ splits the window into two regions ───────
# The workspace layer is a different family from the pane splits above: it
# splits the WINDOW, and each region carries its own pane tree.
WS_SPLITS_BEFORE=$(count_log "split the window into a new workspace region")
focus
shot /output/06-before-workspace-split.png
send_keys ctrl+alt+backslash
if ! wait_for_log_growth "split the window into a new workspace region" "$WS_SPLITS_BEFORE" 15; then
    fail "PHASE 6 FAIL: ctrl+alt+backslash never reached workspace_split_vertical"
fi
sleep 1.5
focus
shot /output/07-after-workspace-split.png
WS_DIFF=$(window_diff /output/06-before-workspace-split.png /output/07-after-workspace-split.png)
if [ "${WS_DIFF:-0}" -lt "$SPLIT_DIFF_MIN" ]; then
    fail "PHASE 6 FAIL: the workspace split changed $WS_DIFF px (min $SPLIT_DIFF_MIN)"
fi
if ! plain_client_log | grep -qE 'split the window into a new workspace region.*regions=2'; then
    fail "PHASE 6 FAIL: the window layout never reported two workspace regions"
fi
echo "PHASE 6 PASS: ctrl+alt+backslash split the window into two regions (+$WS_DIFF px)"

# ── Phase 7: Ctrl+Alt+Left moves focus between regions ───────────
WS_FOCUS_BEFORE=$(count_log "focused workspace moved")
focus
send_keys "$WORKSPACE_FOCUS_LEFT_CHORD"
if ! wait_for_log_growth "focused workspace moved" "$WS_FOCUS_BEFORE" 15; then
    fail "PHASE 7 FAIL: $WORKSPACE_FOCUS_LEFT_CHORD never reached workspace_focus_left"
fi
focus
shot /output/08-workspace-focus-left.png
echo "PHASE 7 PASS: $WORKSPACE_FOCUS_LEFT_CHORD moved focus to the first workspace region"

echo ""
echo "PASS: visual pane-workspace-layout test"
echo "  Inspect screenshots in test-output/:"
echo "    00-single-pane.png             — the adopted pane before any split"
echo "    01-before-split.png            — the window before split_vertical"
echo "    02-after-split.png             — two panes side by side"
echo "    03-typed-right.png             — a marker typed into the new right pane"
echo "    04-typed-left.png              — a marker typed after focus_left"
echo "    05-after-close.png             — back to one pane after close_pane"
echo "    06-before-workspace-split.png  — the window before the workspace split"
echo "    07-after-workspace-split.png   — two workspace regions"
echo "    08-workspace-focus-left.png    — focus back in the first region"
