#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 420
set -euo pipefail

# @lat: [[test#Test Harness#Visual E2E Tests#Tab drag between and out of windows]]
# Every layout below is written in logical pixels; `dev` scales it for a HiDPI
# run (`GPUI_X11_SCALE_FACTOR=2`, see `e2e-visual-tab-drag-cross-window-hidpi`).
. /tests/visual/tab-geometry-common.bash
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
ORACLE=/tests/visual/workspace-tree-oracle.py
oracle() { python3 "$ORACLE" "$RECORD" "$@"; }

fail() {
    echo "FAIL: $1" >&2
    tail -80 "$CLIENT_LOG" >&2 || true
    oracle summary >&2 || true
    exit 1
}

windows() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}

geometry() {
    local info
    info=$(xwininfo -id "$1")
    GX=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    GY=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    GW=$(printf '%s\n' "$info" | awk '/Width:/ { print $2; exit }')
    GH=$(printf '%s\n' "$info" | awk '/Height:/ { print $2; exit }')
}

focus() {
    xdotool windowactivate --sync "$1" 2>/dev/null || xdotool windowfocus --sync "$1" 2>/dev/null || true
    sleep 0.5
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

reset_record() {
    : >"$RECORD"
    sleep 0.3
}

tabs_of() {
    oracle latest-leaf "$1" | python3 -c 'import json,sys; print(*json.load(sys.stdin)["session_ids"])'
}

shown_of() {
    oracle latest-leaf "$1" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["session_ids"][d["active_tab_index"]])'
}

# Centre of tab `index`, in device pixels, in a single-region window `width`
# logical pixels wide holding `count` tabs: a 6px run inset, then equal shares
# capped at 230px. `tab_edge` is the left edge of slot `index`.
tab_slot() {
    local width=$1 count=$2 slot
    slot=$(( (width - 12) / count ))
    [ "$slot" -gt 230 ] && slot=230
    echo "$slot"
}
tab_x() {
    local slot
    slot=$(tab_slot "$2" "$3")
    dev $(( 6 + slot * $1 + slot / 2 ))
}
tab_edge() {
    local slot
    slot=$(tab_slot "$2" "$3")
    dev $(( 6 + slot * $1 ))
}
# The tab strip's vertical middle, inside every tab card.
BAR_Y=$(dev 17)

# Press at a window-local point and travel to a root point in small steps.
# Holds the button at the end; callers release (or press Escape first).
drag_hold() {
    local wid=$1 from_x=$2 from_y=$3 to_x=$4 to_y=$5 step
    geometry "$wid"
    local sx=$((GX + from_x)) sy=$((GY + from_y))
    xdotool mousemove --sync "$sx" "$sy"
    sleep 0.2
    xdotool mousedown 1
    sleep 0.1
    for step in $(seq 1 24); do
        xdotool mousemove --sync "$((sx + (to_x - sx) * step / 24))" "$((sy + (to_y - sy) * step / 24))"
        sleep 0.02
    done
    sleep 0.3
}

wait_window_count() {
    local want=$1
    for _ in $(seq 1 60); do
        mapfile -t mapped < <(windows)
        [ "${#mapped[@]}" -eq "$want" ] && return 0
        sleep 0.3
    done
    return 1
}

# Window A starts with three tabs; window B with two. Both single-region, so no
# workspace pill shifts the tab run.
A_WID=""
for _ in $(seq 1 75); do
    A_WID=$(windows | head -1)
    [ -n "$A_WID" ] && break
    sleep 0.2
done
[ -n "$A_WID" ] || fail "initial Scribe window never mapped"
focus "$A_WID"
A_WS=$(oracle wait-leaves 1 1) || fail "initial workspace never became live"
send_keys ctrl+shift+t
send_keys ctrl+shift+t
oracle wait-latest-leaf-count "$A_WS" 3 >/dev/null || fail "window A never held three tabs"
send_keys ctrl+shift+n
wait_window_count 2 || fail "window B never mapped"
B_WID=""
for wid in "${mapped[@]}"; do [ "$wid" != "$A_WID" ] && B_WID=$wid; done
xdotool windowsize "$A_WID" "$(dev 800)" "$(dev 600)"
xdotool windowmove "$A_WID" "$(dev 80)" "$(dev 180)"
xdotool windowsize "$B_WID" "$(dev 800)" "$(dev 600)"
xdotool windowmove "$B_WID" "$(dev 1040)" "$(dev 180)"
sleep 1
focus "$B_WID"
B_WS=""
for _ in $(seq 1 50); do
    B_WS=$(oracle known-workspaces | tr ' ' '\n' | grep -vx "$A_WS" | tail -1 || true)
    [ -n "$B_WS" ] && oracle latest-leaf "$B_WS" >/dev/null 2>&1 && break
    sleep 0.2
done
[ -n "$B_WS" ] || fail "window B's workspace never reported"
send_keys ctrl+shift+t
oracle wait-latest-leaf-count "$B_WS" 2 >/dev/null || fail "window B never held two tabs"

# PHASE 1: tear window A's middle tab out past its bottom edge. One TransferTab,
# a third window maps holding exactly that tab, and A keeps the other two.
focus "$A_WID"
MOVED=$(tabs_of "$A_WS" | awk '{ print $2 }')
reset_record
geometry "$A_WID"
drag_hold "$A_WID" "$(tab_x 1 800 3)" "$BAR_Y" "$((GX + $(dev 400)))" "$((GY + $(dev 660)))"
xdotool mouseup 1
wait_window_count 3 || fail "tear-out did not map a new window"
oracle assert-tab-transfer "$MOVED" >/output/tab-tearout.json || fail "tear-out did not use one TransferTab"
oracle wait-latest-leaf-count "$A_WS" 2 >/dev/null || fail "window A did not lose the torn-out tab"
C_WID=""
for wid in "${mapped[@]}"; do [ "$wid" != "$A_WID" ] && [ "$wid" != "$B_WID" ] && C_WID=$wid; done
[ -n "$C_WID" ] || fail "could not identify the torn-out window"
C_TAB=$MOVED
# The tabs left behind must still be painted as tabs. A release outside the
# window ended the root's drag but not the titlebar's, which kept painting the
# tab that moved into the dragged slot as the blank recess of a tab in hand.
focus "$A_WID"
sleep 0.5
import -window "$A_WID" +repage /output/tab-tearout-source.png
for index in 0 1; do
    ink=$(convert /output/tab-tearout-source.png \
        -crop "$(dev 150)x$(dev 20)+$(( $(tab_edge "$index" 800 2) + $(dev 8) ))+$(dev 8)" +repage \
        -colorspace Gray -threshold 25% -fuzz 5% -trim +repage -format "%w" info: 2>/dev/null || echo 0)
    [ "${ink%.*}" -ge "$(dev 12)" ] || fail "tab $index was left blank after the tear-out (ink ${ink:-0})"
done
echo "PHASE 1 PASS: tab tore out into its own window"

# PHASE 2: drag A's first tab onto B's bar between its two tabs. The tab lands
# at slot 1, B shows it, and B's window takes focus.
MOVED=$(tabs_of "$A_WS" | awk '{ print $1 }')
reset_record
geometry "$B_WID"
drag_hold "$A_WID" "$(tab_x 0 800 2)" "$BAR_Y" "$((GX + $(tab_edge 1 800 2)))" "$((GY + BAR_Y))"
xdotool mouseup 1
oracle wait-latest-leaf-count "$B_WS" 3 >/dev/null || fail "window B did not gain the dropped tab"
oracle assert-cross-window-tab-move "$A_WS" "$B_WS" "$MOVED" >/output/tab-cross-window-bar.json \
    || fail "bar drop did not use one atomic cross-window move"
[ "$(tabs_of "$B_WS" | awk '{ print $2 }')" = "$MOVED" ] || fail "bar drop did not land at the marked slot"
for _ in $(seq 1 25); do [ "$(shown_of "$B_WS")" = "$MOVED" ] && break; sleep 0.2; done
[ "$(shown_of "$B_WS")" = "$MOVED" ] || fail "destination did not show the dropped tab"
for _ in $(seq 1 25); do [ "$(xdotool getactivewindow)" = "$B_WID" ] && break; sleep 0.2; done
[ "$(xdotool getactivewindow)" = "$B_WID" ] || fail "destination window did not take focus"
echo "PHASE 2 PASS: bar drop landed at its slot, shown and focused"
# Captured now: later phases reset the record, and window A reports nothing
# while a held drag is cancelled. Wait for A's own post-move report first; B's
# arrives on a different connection and can lead it.
oracle wait-latest-leaf-count "$A_WS" 1 >/dev/null || fail "window A never reported losing the dropped tab"
A_LAST=$(tabs_of "$A_WS")

# PHASE 3: hold A's now-sole tab over B, then Escape. Over a sibling the sole
# tab targets it rather than moving its own window; Escape sends nothing.
focus "$A_WID"
geometry "$B_WID"
reset_record
geometry "$B_WID"
drag_hold "$A_WID" "$(tab_x 0 800 1)" "$BAR_Y" "$((GX + $(dev 400)))" "$((GY + $(dev 300)))"
# Not send_keys: --clearmodifiers would release the held button first.
xdotool key Escape
sleep 0.3
xdotool mouseup 1
sleep 0.6
[ "$(oracle count MoveWorkspace)" -eq 0 ] || fail "Escape committed a cross-window tab move"
[ "$(oracle count TransferTab)" -eq 0 ] || fail "Escape tore the held tab out"
geometry "$A_WID"
[ "$GX" -lt "$(dev 200)" ] || fail "Escape over a sibling moved the source window"
echo "PHASE 3 PASS: Escape over a sibling window cancelled cleanly"

# PHASE 4: drop A's sole tab onto B's panes. It joins B as its last tab and A,
# now empty, closes.
MOVED=$A_LAST
reset_record
geometry "$B_WID"
drag_hold "$A_WID" "$(tab_x 0 800 1)" "$BAR_Y" "$((GX + $(dev 400)))" "$((GY + $(dev 300)))"
xdotool mouseup 1
oracle wait-latest-leaf-count "$B_WS" 4 >/dev/null || fail "window B did not gain the pane-dropped tab"
oracle assert-cross-window-tab-move "$A_WS" "$B_WS" "$MOVED" source-closed >/output/tab-cross-window-pane.json \
    || fail "pane drop did not use one atomic cross-window move"
[ "$(tabs_of "$B_WS" | awk '{ print $NF }')" = "$MOVED" ] || fail "pane drop did not append the tab"
for _ in $(seq 1 25); do [ "$(shown_of "$B_WS")" = "$MOVED" ] && break; sleep 0.2; done
[ "$(shown_of "$B_WS")" = "$MOVED" ] || fail "pane drop did not show the dropped tab"
B_SHOWN=$MOVED
wait_window_count 2 || fail "emptied source window did not close"
echo "PHASE 4 PASS: a window's last tab moved into a sibling and the empty window closed"

# PHASE 5: drag the torn-out window's only tab down out of it. No transaction:
# the window itself follows the pointer.
# The torn-out window opened at phase 1's release point, which can leave it
# hanging off the bottom of the screen; park it where the pointer can leave it.
xdotool windowsize "$C_WID" "$(dev 700)" "$(dev 400)"
xdotool windowmove "$C_WID" "$(dev 80)" "$(dev 180)"
sleep 1
focus "$C_WID"
geometry "$C_WID"
C_X=$GX
C_Y=$GY
reset_record
drag_hold "$C_WID" "$(tab_x 0 "$((GW / E2E_SCALE))" 1)" "$BAR_Y" "$((GX + $(dev 200)))" "$((GY + GH + $(dev 150)))"
xdotool mouseup 1
sleep 1
geometry "$C_WID"
[ "$(oracle count TransferTab)" -eq 0 ] && [ "$(oracle count MoveWorkspace)" -eq 0 ] \
    || fail "a sole-tab drag sent a transaction instead of moving its window"
[ "$((GX - C_X))" -gt "$(dev 40)" ] || [ "$((GY - C_Y))" -gt "$(dev 40)" ] \
    || fail "sole-tab drag did not move its window ($C_X,$C_Y -> $GX,$GY)"
echo "PHASE 5 PASS: a window's only tab carried its window instead of tearing out"

# PHASE 6: the palette's non-drag path tears B's shown tab out too.
focus "$B_WID"
MOVED=$B_SHOWN
reset_record
send_keys ctrl+shift+p
xdotool type --clearmodifiers --delay 8 "Move tab to new window"
sleep 0.3
send_keys Return
wait_window_count 3 || fail "palette tear-out did not map a new window"
oracle assert-tab-transfer "$MOVED" >/output/tab-palette-tearout.json || fail "palette tear-out did not use TransferTab"
echo "PHASE 6 PASS: palette moved the shown tab to a new window"
D_WID=""
for wid in "${mapped[@]}"; do [ "$wid" != "$B_WID" ] && [ "$wid" != "$C_WID" ] && D_WID=$wid; done
[ -n "$D_WID" ] || fail "could not identify the palette's new window"
D_TAB=$MOVED

# PHASE 7: overlapping windows. C and D overlap, away from B. A tab dropped
# where they overlap must land in whichever is on top: the drop target used to
# be the first window, by id, whose rect held the pointer, so one of these two
# drops went into the window underneath.
xdotool windowsize "$C_WID" "$(dev 700)" "$(dev 500)"
xdotool windowmove "$C_WID" "$(dev 80)" "$(dev 180)"
xdotool windowsize "$D_WID" "$(dev 650)" "$(dev 450)"
xdotool windowmove "$D_WID" "$(dev 330)" "$(dev 330)"
sleep 1
OVERLAP_X=$(dev 550)
OVERLAP_Y=$(dev 500)
# A tab lives in exactly one workspace, so joining the top window's own tab is
# proof it did not go to the one underneath.
for top in "$D_WID" "$C_WID"; do
    if [ "$top" = "$D_WID" ]; then ANCHOR=$D_TAB; else ANCHOR=$C_TAB; fi
    focus "$top"
    focus "$B_WID"
    oracle latest-leaf "$B_WS" >/dev/null || fail "window B's tabs are not on the record"
    MOVED=$(tabs_of "$B_WS" | awk '{ print $1 }')
    COUNT=$(tabs_of "$B_WS" | wc -w)
    reset_record
    drag_hold "$B_WID" "$(tab_x 0 800 "$COUNT")" "$BAR_Y" "$OVERLAP_X" "$OVERLAP_Y"
    xdotool mouseup 1
    oracle wait-tabs-together "$ANCHOR" "$MOVED" >/dev/null \
        || fail "a drop where two windows overlap did not land in the one on top ($top)"
    oracle wait-latest-leaf-count "$B_WS" "$((COUNT - 1))" >/dev/null \
        || fail "window B did not report losing the dropped tab"
done
echo "PHASE 7 PASS: a drop where windows overlap lands in the topmost one"
echo "PASS: tab drag between and out of windows"
