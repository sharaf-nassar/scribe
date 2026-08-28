#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 300
set -euo pipefail

# @lat: [[test#Test Harness#Visual E2E Tests#Tab drag reorders in every bar]]
. /tests/visual/tab-geometry-common.bash

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
ORACLE=/tests/visual/workspace-tree-oracle.py
oracle() { python3 "$ORACLE" "$RECORD" "$@"; }

fail() {
    echo "FAIL: $1" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    oracle summary >&2 || true
    exit 1
}

find_window() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}

focus() {
    WID=$(find_window | head -1)
    [ -n "$WID" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.6
    info=$(xwininfo -id "$WID")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/Width:/ { print $2; exit }')
    WIN_H=$(printf '%s\n' "$info" | awk '/Height:/ { print $2; exit }')
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

reset_record() {
    : >"$RECORD"
    sleep 0.2
}

leaf_tabs() {
    oracle leaf "$1" | python3 -c 'import json,sys; print(*json.load(sys.stdin)["session_ids"])'
}

tab_center() {
    local index=$1 width=$2 count=$3
    echo $(( index * width / count + width / (2 * count) ))
}

bar_width() {
    local offset=$1 output=$2
    import -window "$WID" +repage miff:- \
        | convert - -crop "${WIN_W}x34+0+${offset}" +repage "$output"
    local width
    width=$(band_ink_width "$output")
    width=${width%.*}
    [ "${width:-0}" -gt 0 ] || fail "could not measure painted tab strip"
    printf '%s\n' "$width"
}

drag() {
    local from_x=$1 from_y=$2 to_x=$3 to_y=$4 step
    xdotool mousemove --sync "$((WIN_X + from_x))" "$((WIN_Y + from_y))"
    sleep 0.2
    xdotool mousedown 1
    sleep 0.2
    xdotool mousemove_relative --sync -- 3 0
    sleep 0.2
    for step in $(seq 1 12); do
        xdotool mousemove --sync \
            "$((WIN_X + from_x + (to_x - from_x) * step / 12))" \
            "$((WIN_Y + from_y + (to_y - from_y) * step / 12))"
        sleep 0.05
    done
    xdotool mouseup 1
    sleep 0.8
}

# The top strip starts with three tabs. Its source id is retained through both
# direction changes, making the wire operation and not a merely changed count
# the oracle.
for _ in $(seq 1 40); do
    [ -n "$(find_window | head -1)" ] && break
    sleep 0.5
done
focus
oracle wait-leaves 1 1 >/dev/null || fail "initial workspace never became live"
TOP=$(oracle wait-leaves 1 1)
send_keys ctrl+shift+t
send_keys ctrl+shift+t
oracle wait-leaf-session-count "$TOP" 3 >/dev/null || fail "top region never received three tabs"
TOP_BEFORE=$(leaf_tabs "$TOP")
MOVED=$(printf '%s\n' "$TOP_BEFORE" | awk '{ print $1 }')

# A horizontal workspace split creates the lower in-region bar; focus that
# pane before adding tabs so its bar has a real insertion target.
send_keys ctrl+alt+minus
ids=$(oracle wait-leaves 2 1) || fail "lower workspace never became live"
read -r TOP LOWER <<<"$ids"
xdotool mousemove --sync "$((WIN_X + 200))" "$((WIN_Y + WIN_H * 6 / 8))"
xdotool click 1
send_keys ctrl+shift+t
send_keys ctrl+shift+t
oracle wait-leaf-session-count "$LOWER" 3 >/dev/null || fail "lower region never received three tabs"
TOP_WIDTH=$(bar_width 0 /output/tab-cross-region-top.png)
LOWER_Y=$((WIN_H / 2 + 17))
LOWER_WIDTH=$(bar_width "$((WIN_H / 2))" /output/tab-cross-region-lower.png)
TOP_COUNT=$(leaf_tabs "$TOP" | wc -w)
LOWER_COUNT=$(leaf_tabs "$LOWER" | wc -w)
LOWER_MOVED=$(leaf_tabs "$LOWER" | awk '{ print $1 }')

# Escape must roll back the transient same-bar reorder and send no transaction.
# It is deliberately separate so a failure in cancellation cannot hide the
# user-reachable transfer below.
reset_record
from_x=$(tab_center 0 "$TOP_WIDTH" "$TOP_COUNT")
xdotool mousemove --sync "$((WIN_X + from_x))" "$((WIN_Y + 17))"
xdotool mousedown 1
xdotool mousemove_relative --sync -- 3 0
xdotool mousemove --sync "$((WIN_X + $(tab_center 0 "$LOWER_WIDTH" "$LOWER_COUNT")))" "$((WIN_Y + LOWER_Y))"
sleep 0.2
send_keys Escape
xdotool mouseup 1
sleep 0.5
[ "$(oracle count MoveWorkspace)" -eq 0 ] || fail "Escape committed a tab-subtree move"

# The first cross-region commit moves a complete tab subtree from the lower bar
# to titlebar chrome. Exactly one same-window refresh and no replacement session
# may accompany it.
reset_record
from_x=$(tab_center 0 "$LOWER_WIDTH" "$LOWER_COUNT")
drag "$from_x" "$LOWER_Y" "$(tab_center 0 "$TOP_WIDTH" "$TOP_COUNT")" 17
oracle wait-leaf-session-count "$TOP" "$((TOP_COUNT + 1))" >/dev/null \
    || fail "top region did not gain the dragged tab"
oracle wait-leaf-session-count "$LOWER" "$((LOWER_COUNT - 1))" >/dev/null \
    || fail "lower region did not lose the dragged tab"
oracle assert-tab-subtree-move "$LOWER" "$TOP" "$LOWER_MOVED" \
    >/output/tab-cross-region-lower-to-top.json \
    || fail "lower-to-top transfer did not use one atomic workspace move"
leaf_tabs "$TOP" | tr ' ' '\n' | grep -Fx "$LOWER_MOVED" >/dev/null \
    || fail "lower-to-top transfer lost the selected tab identity"
echo "PHASE 1 PASS: lower-region tab moved atomically into titlebar chrome"
echo "PASS: tab subtree moves from lower-region bar into titlebar chrome"
