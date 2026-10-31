#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 300
set -euo pipefail

# @lat: [[test#Test Harness#Visual E2E Tests#Tab drag on Wayland]]
# Runs under SCRIBE_DISPLAY_SERVER=wayland (`just e2e-visual-tab-drag-wayland`):
# the client is a Wayland client of a real sway. The pointer is driven with
# xdotool on the Xvfb that hosts sway's output window: sway's X11 backend turns
# those into its seat's own pointer input, so every press, motion, and release
# reaches the client the way a user's would, serials included. `swaymsg seat
# cursor set` is not a substitute: it moves the cursor without delivering
# motion to the surface holding an implicit grab, so a held drag never moves.
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
ORACLE=/tests/visual/workspace-tree-oracle.py
oracle() { python3 "$ORACLE" "$RECORD" "$@"; }

fail() {
    echo "FAIL: $1" >&2
    grim /output/tab-drag-wayland-fail.png 2>/dev/null || true
    tail -60 "$CLIENT_LOG" >&2 || true
    tail -20 /output/sway.log >&2 || true
    oracle summary >&2 || true
    exit 1
}

[ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${SWAYSOCK:-}" ] \
    || fail "not running under the Wayland harness (SCRIBE_DISPLAY_SERVER=wayland)"

# `con_id x y width height` for every Scribe toplevel, in sway's logical layout.
scribe_windows() {
    swaymsg -t get_tree | python3 -c '
import json, sys
def walk(node):
    if node.get("app_id") == "scribe":
        r = node["rect"]
        print(node["id"], r["x"], r["y"], r["width"], r["height"])
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        walk(child)
walk(json.load(sys.stdin))'
}

window_count() { scribe_windows | wc -l; }

wait_window_count() {
    for _ in $(seq 1 60); do
        [ "$(window_count)" -eq "$1" ] && return 0
        sleep 0.25
    done
    return 1
}

rect_of() {
    read -r _ WX WY WW WH < <(scribe_windows | awk -v id="$1" '$1 == id')
}

# sway's single output is the Xvfb root window at +0+0, so its layout
# coordinates are root coordinates.
[ -n "${DISPLAY:-}" ] || fail "no X display hosting sway's output"

# Press at a layout point and travel to another in small steps, holding.
drag_hold() {
    local from_x=$1 from_y=$2 to_x=$3 to_y=$4 step
    xdotool mousemove --sync "$from_x" "$from_y"
    sleep 0.3
    xdotool mousedown 1
    sleep 0.1
    for step in $(seq 1 30); do
        xdotool mousemove "$((from_x + (to_x - from_x) * step / 30))" "$((from_y + (to_y - from_y) * step / 30))"
        sleep 0.03
    done
    sleep 0.3
}

# Centre of the first tab of a single-region window `width` wide holding
# `count` tabs: a 6px run inset, then equal shares capped at 230px.
first_tab_x() {
    local slot=$(( ($1 - 12) / $2 ))
    [ "$slot" -gt 230 ] && slot=230
    echo $(( 6 + slot / 2 ))
}

wait_window_count 1 || fail "the Scribe window never mapped under sway"
oracle wait-leaves 1 1 >/dev/null || fail "initial workspace never became live"
WS=$(oracle wait-leaves 1 1)
CON=$(scribe_windows | awk 'NR == 1 { print $1 }')
swaymsg -q "[con_id=$CON] resize set 800 500, move position 100 100"
sleep 1
rect_of "$CON"
[ "$WX" -eq 100 ] && [ "$WY" -eq 100 ] || fail "sway did not place the window ($WX,$WY)"

# PHASE 1: a window's only tab, dragged out past the bottom edge, carries the
# window: the client hands the press to the compositor (`xdg_toplevel.move`)
# and sway moves the window with the pointer from there. No transaction.
: >"$RECORD"
TAB_X=$((WX + $(first_tab_x "$WW" 1)))
TAB_Y=$((WY + 17))
drag_hold "$TAB_X" "$TAB_Y" "$((WX + 400))" "$((WY + WH + 80))"
# Keep going: once the compositor owns the move, this carries the window.
for step in $(seq 1 20); do
    xdotool mousemove "$((WX + 400 + step * 10))" "$((WY + WH + 80 + step * 5))"
    sleep 0.03
done
sleep 0.3
xdotool mouseup 1
sleep 1
FROM_X=$WX FROM_Y=$WY
rect_of "$CON"
[ "$(window_count)" -eq 1 ] || fail "the sole-tab drag opened another window"
[ "$(oracle count TransferTab)" -eq 0 ] && [ "$(oracle count MoveWorkspace)" -eq 0 ] \
    || fail "the sole-tab drag sent a transaction instead of moving its window"
[ "$((WX - FROM_X))" -gt 100 ] && [ "$((WY - FROM_Y))" -gt 50 ] \
    || fail "the sole-tab drag did not carry its window ($FROM_X,$FROM_Y -> $WX,$WY)"
grep -q "handing the window to the compositor move" "$CLIENT_LOG" \
    || fail "the client never handed the press to the compositor"
echo "PHASE 1 PASS: a window's only tab carried its window through the compositor move"

# PHASE 2: with two tabs, dragging one out tears it into a new window. Wayland
# hides window positions, so the compositor places the window; the outcome is
# one TransferTab, a second toplevel, and the source keeping its other tab.
swaymsg -q "[con_id=$CON] move position 100 100"
sleep 0.5
# No X keyboard reaches a Wayland client, so the tab comes from the CLI. The
# test daemon holds a window of its own; the client's is the other one.
DAEMON_WINDOW=$(scribe-test daemon window-id)
CLIENT_WINDOW=$(scribe windows | awk -v daemon="$DAEMON_WINDOW" '$1 != daemon { print $1; exit }')
[ -n "$CLIENT_WINDOW" ] || fail "could not name the client's window"
scribe action --window "$CLIENT_WINDOW" new-tab >/dev/null || fail "could not open a second tab"
oracle wait-latest-leaf-count "$WS" 2 >/dev/null || fail "the window never held two tabs"
MOVED=$(oracle latest-leaf "$WS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["session_ids"][0])')
rect_of "$CON"
: >"$RECORD"
drag_hold "$((WX + $(first_tab_x "$WW" 2)))" "$((WY + 17))" "$((WX + 300))" "$((WY + WH + 80))"
xdotool mouseup 1
wait_window_count 2 || fail "tear-out did not map a second toplevel"
oracle assert-tab-transfer "$MOVED" >/output/tab-wayland-tearout.json \
    || fail "tear-out did not use one TransferTab"
oracle wait-latest-leaf-count "$WS" 1 >/dev/null || fail "the source did not lose the torn-out tab"
echo "PHASE 2 PASS: a tab tore out into a compositor-placed window"
echo "PASS: tab drags on Wayland"
