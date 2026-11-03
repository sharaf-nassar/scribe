#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container (use just e2e-visual-tab-mirror)." >&2; exit 99; }
# Scripted E2E: a tab's hover magnifier opens a mirror window that repaints
# with the tab, takes keystrokes for the same session, starts kept on top, and
# lets its chrome switch release that.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1, so the client's tab and
# `scribe-test send` / `wait-output` address the same session.
set -euo pipefail

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"
TITLEBAR_HEIGHT=36
# The mirror's slim strip and its right-hand controls, from mirror_window.rs:
# 4px right padding, an 18px close target, a 4px gap, then an 18px pin.
# This container's X11 has no compositor, so the window keeps the window
# manager's frame and the strip starts at the client area's top-left.
MIRROR_HEADER_HEIGHT=22
CLOSE_FROM_RIGHT=13
PIN_FROM_RIGHT=35

fail() {
    echo "FAIL: $1" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

wait_for() {
    local tries="$1"
    shift
    for _ in $(seq 1 "$tries"); do
        "$@" && return 0
        sleep 0.25
    done
    return 1
}

mirror_window() { xdotool search --name '^Mirror: ' 2>/dev/null | head -1; }
mirror_above() { xprop -id "$MWID" _NET_WM_STATE 2>/dev/null | grep -q _NET_WM_STATE_ABOVE; }
mirror_not_above() { ! mirror_above; }
mirror_gone() { [ -z "$(mirror_window)" ]; }
mirror_open() { [ -n "$(mirror_window)" ]; }
# The log line follows GPUI's first draw of the window, so it can trail the map.
mirror_logged() { [ "$(grep -c "opened a tab mirror" "$CLIENT_LOG" 2>/dev/null || true)" -gt "$opened_before" ]; }

# `x y width height` of a window's client area in root coordinates.
geometry() {
    xwininfo -id "$1" | awk '
        /Absolute upper-left X/ { x = $NF }
        /Absolute upper-left Y/ { y = $NF }
        /^ *Width:/ { w = $NF }
        /^ *Height:/ { h = $NF }
        END { print x, y, w, h }'
}

# Lit pixels that differ between two captures inside one crop.
diff_ink() {
    convert "$1" "$2" -compose difference -composite -crop "$3" +repage \
        -colorspace Gray -threshold 10% -format '%[fx:mean*w*h]' info:
}

sleep 0.8
WID=$(xdotool search --name '^Scribe$' | head -1) || true
[ -n "$WID" ] || fail "no Scribe window"
xdotool windowfocus --sync "$WID" 2>/dev/null || true
read -r WIN_X WIN_Y WIN_W WIN_H <<<"$(geometry "$WID")"

# Phase 1: the magnifier appears only while the tab is hovered. The diff of a
# resting and a hovered capture of the titlebar is exactly the mark.
xdotool mousemove "$((WIN_X + WIN_W / 2))" "$((WIN_Y + WIN_H / 2))"
sleep 0.5
import -window "$WID" +repage /output/tab-mirror-rest.png
xdotool mousemove "$((WIN_X + 60))" "$((WIN_Y + TITLEBAR_HEIGHT / 2))"
sleep 0.5
import -window "$WID" +repage /output/tab-mirror-hover.png
read -r MARK_W MARK_H MARK_X MARK_Y <<<"$(
    convert /output/tab-mirror-rest.png /output/tab-mirror-hover.png -compose difference \
        -composite -crop "${WIN_W}x${TITLEBAR_HEIGHT}+0+0" +repage -colorspace Gray \
        -threshold 10% -trim -format '%w %h %X %Y' info:
)"
MARK_X=${MARK_X#+}
MARK_Y=${MARK_Y#+}
[ "${MARK_W:-0}" -ge 6 ] && [ "${MARK_W:-0}" -le 20 ] && [ "${MARK_H:-0}" -ge 6 ] \
    || fail "phase 1: hovering the tab did not reveal a magnifier-sized mark (${MARK_W}x${MARK_H})"
echo "phase 1: magnifier revealed at ${MARK_X},${MARK_Y} (${MARK_W}x${MARK_H})"

# Phase 2: clicking it opens a mirror centred over the window, kept on top.
opened_before=$(grep -c "opened a tab mirror" "$CLIENT_LOG" 2>/dev/null || true)
xdotool mousemove "$((WIN_X + MARK_X + MARK_W / 2))" "$((WIN_Y + MARK_Y + MARK_H / 2))"
sleep 0.3
xdotool click 1
wait_for 40 mirror_open || fail "phase 2: no mirror window opened"
MWID=$(mirror_window)
wait_for 40 mirror_logged || fail "phase 2: the client never logged the mirror"
sleep 0.8
read -r M_X M_Y M_W M_H <<<"$(geometry "$MWID")"
dx=$(((M_X + M_W / 2) - (WIN_X + WIN_W / 2)))
dy=$(((M_Y + M_H / 2) - (WIN_Y + WIN_H / 2)))
[ "${dx#-}" -le 48 ] && [ "${dy#-}" -le 48 ] \
    || fail "phase 2: the mirror is not centred over its window (offset ${dx},${dy})"
wait_for 20 mirror_above || fail "phase 2: the mirror is not kept on top"
echo "phase 2: mirror ${M_W}x${M_H} centred (offset ${dx},${dy}) and kept on top"

# Phase 3: output in the tab repaints the mirror.
GRID_CROP="${M_W}x$((M_H - MIRROR_HEADER_HEIGHT))+0+${MIRROR_HEADER_HEIGHT}"
import -window "$MWID" +repage /output/tab-mirror-before.png
scribe-test send "$SESSION" 'printf "MIRROR_OUT_%s\\n" 1 2 3 4 5 6\n'
scribe-test wait-output "$SESSION" MIRROR_OUT_6 --timeout 15000 >/dev/null \
    || fail "phase 3: the session never printed"
sleep 0.8
import -window "$MWID" +repage /output/tab-mirror-after.png
ink=$(diff_ink /output/tab-mirror-before.png /output/tab-mirror-after.png "$GRID_CROP")
[ "${ink%.*}" -ge 200 ] || fail "phase 3: the mirror did not repaint the tab's output (${ink%.*} px)"
echo "phase 3: the mirror repainted ${ink%.*} px of new output"

# Phase 4: keystrokes typed into the mirror reach the same session.
xdotool windowfocus --sync "$MWID" 2>/dev/null || true
sleep 0.3
xdotool type --delay 30 'echo MIRROR_TYPED_$((6*7))'
xdotool key Return
scribe-test wait-output "$SESSION" MIRROR_TYPED_42 --timeout 15000 >/dev/null \
    || fail "phase 4: keystrokes typed into the mirror never reached the session"
echo "phase 4: typing in the mirror ran a command in the tab's session"

# Phase 5: the chrome switch releases and restores keep-on-top.
pin_x=$((M_X + M_W - PIN_FROM_RIGHT))
pin_y=$((M_Y + MIRROR_HEADER_HEIGHT / 2))
xdotool mousemove "$pin_x" "$pin_y"
sleep 0.3
xdotool click 1
wait_for 20 mirror_not_above || fail "phase 5: the pin switch did not release keep-on-top"
xdotool click 1
wait_for 20 mirror_above || fail "phase 5: the pin switch did not restore keep-on-top"
echo "phase 5: the pin switch toggles keep-on-top"

# Phase 5b: a mirror taller than the tab fills from scrollback down to the
# live prompt instead of painting the tab's screen at the top and blank below.
scribe-test send "$SESSION" 'clear; seq -f "MIRROR_FILL_%03g" 1 150\n'
scribe-test wait-output "$SESSION" MIRROR_FILL_150 --timeout 15000 >/dev/null \
    || fail "phase 5b: the session never printed the fill"
# Moved to the top first: a capture clips at the screen edge, so a window that
# runs past the bottom of the screen would hide exactly the rows under test.
xdotool windowmove "$MWID" "$M_X" 0
xdotool windowsize "$MWID" "$M_W" "$((WIN_H + 220))"
sleep 1.2
import -window "$MWID" +repage /output/tab-mirror-tall.png
TALL_H=$(identify -format '%h' /output/tab-mirror-tall.png)
[ "$TALL_H" -gt "$((WIN_H + 100))" ] \
    || fail "phase 5b: the mirror did not grow well past the tab ($TALL_H px)"
# The bottom 200px of the grid lie below where the tab's own screen ends.
bottom_ink=$(convert /output/tab-mirror-tall.png -crop "${M_W}x200+0+$((TALL_H - 210))" +repage \
    -colorspace Gray -threshold 25% -format '%[fx:mean*w*h]' info:)
[ "${bottom_ink%.*}" -ge 300 ] \
    || fail "phase 5b: the tall mirror left its bottom blank (${bottom_ink%.*} lit px)"
echo "phase 5b: a mirror taller than the tab fills to its bottom (${bottom_ink%.*} lit px)"

# Phase 6: a resized mirror re-wraps rather than resizing the tab's PTY.
resizes_before=$(grep -c "published a pane's grid size" "$CLIENT_LOG" || true)
xdotool windowsize "$MWID" 420 260
sleep 1
import -window "$MWID" +repage /output/tab-mirror-narrow.png
[ "$(grep -c "published a pane's grid size" "$CLIENT_LOG" || true)" -eq "$resizes_before" ] \
    || fail "phase 6: resizing the mirror resized the tab's PTY"
pgrep -x scribe-client >/dev/null || fail "phase 6: the client exited"
echo "phase 6: resizing the mirror left the tab's grid alone"

# Phase 7: the strip's close button closes the mirror and only the mirror.
read -r C_X C_Y C_W _ <<<"$(geometry "$MWID")"
xdotool mousemove "$((C_X + C_W - CLOSE_FROM_RIGHT))" "$((C_Y + MIRROR_HEADER_HEIGHT / 2))"
sleep 0.3
xdotool click 1
wait_for 40 mirror_gone || fail "phase 7: the strip's close button left the mirror open"
xdotool search --name '^Scribe$' >/dev/null || fail "phase 7: closing the mirror closed the terminal"
echo "phase 7: the strip's close button closed the mirror"

# Phase 8: the session leaving takes a reopened mirror with it.
xdotool windowfocus --sync "$WID" 2>/dev/null || true
opened_before=$(grep -c "opened a tab mirror" "$CLIENT_LOG" 2>/dev/null || true)
xdotool mousemove "$((WIN_X + 60))" "$((WIN_Y + TITLEBAR_HEIGHT / 2))"
sleep 0.5
xdotool mousemove "$((WIN_X + MARK_X + MARK_W / 2))" "$((WIN_Y + MARK_Y + MARK_H / 2))"
sleep 0.3
xdotool click 1
wait_for 40 mirror_open || fail "phase 8: the magnifier did not reopen a mirror"
# Phase 6 left the last mirror at 420x260, and phase 5b dragged it to the top
# of the screen: the size comes back, the position does not.
MWID=$(mirror_window)
sleep 0.8
read -r R_X R_Y R_W R_H <<<"$(geometry "$MWID")"
[ "$((R_W - 420))" -ge -2 ] && [ "$((R_W - 420))" -le 2 ] \
    && [ "$((R_H - 260))" -ge -2 ] && [ "$((R_H - 260))" -le 2 ] \
    || fail "phase 8: the reopened mirror is ${R_W}x${R_H}, not the resized 420x260"
dx=$(((R_X + R_W / 2) - (WIN_X + WIN_W / 2)))
dy=$(((R_Y + R_H / 2) - (WIN_Y + WIN_H / 2)))
[ "${dx#-}" -le 48 ] && [ "${dy#-}" -le 48 ] \
    || fail "phase 8: the reopened mirror kept a dragged position (offset ${dx},${dy})"
SAVED=$(cat "${XDG_STATE_HOME:?}"/*/mirror_state.toml 2>/dev/null || true)
grep -q '^width = 420$' <<<"$SAVED" && grep -q '^height = 260$' <<<"$SAVED" \
    || fail "phase 8: the resized size was not saved to disk (${SAVED:-no file})"
echo "phase 8: a new mirror opens at the resized ${R_W}x${R_H}, centred (offset ${dx},${dy})"
scribe-test send "$SESSION" 'exit\n'
wait_for 40 mirror_gone || fail "phase 8: the mirror outlived its session"
echo "PASS: tab mirror opens, mirrors, types, pins, and closes with its session"
