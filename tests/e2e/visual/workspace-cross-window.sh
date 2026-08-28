#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 420
set -euo pipefail

# @lat: [[test#GPUI Workspace Drag]]
. /tests/visual/tab-geometry-common.bash

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
ORACLE=/tests/visual/workspace-tree-oracle.py
PILL_CENTER_X=44
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

focus_window() {
    local wid=$1 info
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.6
    info=$(xwininfo -id "$wid")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/Width:/ { print $2; exit }')
    WIN_H=$(printf '%s\n' "$info" | awk '/Height:/ { print $2; exit }')
}

remember_source_geometry() {
    focus_window "$SOURCE_WID"
    import -window "$SOURCE_WID" +repage /output/workspace-cross-source.png
    S_X=$WIN_X; S_Y=$WIN_Y; S_W=$WIN_W; S_H=$WIN_H
    S_TOP=$(measure_bar_height /output/workspace-cross-source.png 0 100 workspace-cross-source)
    S_STATUS=$(measure_status_height /output/workspace-cross-source.png "$((S_H - 100))" 100 workspace-cross-source)
    S_GRID_H=$((S_H - S_TOP - S_STATUS))
}

remember_target_geometry() {
    focus_window "$TARGET_WID"
    import -window "$TARGET_WID" +repage /output/workspace-cross-target.png
    T_X=$WIN_X; T_Y=$WIN_Y; T_W=$WIN_W; T_H=$WIN_H
    T_TOP=$(measure_bar_height /output/workspace-cross-target.png 0 100 workspace-cross-target)
    T_STATUS=$(measure_status_height /output/workspace-cross-target.png "$((T_H - 100))" 100 workspace-cross-target)
    T_GRID_H=$((T_H - T_TOP - T_STATUS))
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

palette() {
    focus_window "$SOURCE_WID"
    send_keys ctrl+shift+p
    xdotool type --clearmodifiers --delay 8 "$1"
    sleep 0.3
    send_keys Return
}

reset_record() {
    : >"$RECORD"
    sleep 0.3
}

last_move() {
    python3 - "$RECORD" <<'PY'
import json, sys
found = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if row.get("dir") == "client" and message.get("type") == "MoveWorkspace":
        found = message
if found is None:
    raise SystemExit(1)
print(found["workspace_id"], found["target_workspace_id"])
PY
}

assert_move() {
    local want_source=$1 operation=$2 source target
    read -r source target <<<"$(last_move)" || fail "no workspace move reached the wire"
    [ "$want_source" = "-" ] || [ "$source" = "$want_source" ] \
        || fail "move source $source did not match focused workspace $want_source"
    oracle assert-workspace-move "$source" "$target" "$operation" \
        >"/output/workspace-cross-$operation.json" \
        || fail "$operation move lacked atomic refresh/result/no-create evidence"
}

drag_hold_to_target() {
    local target_x=$1 target_y=$2 step
    xdotool mousemove --sync "$((S_X + PILL_CENTER_X))" "$((S_Y + S_TOP / 2))"
    xdotool mousedown 1
    xdotool mousemove_relative --sync -- 3 0
    for step in $(seq 1 12); do
        xdotool mousemove --sync \
            "$((S_X + PILL_CENTER_X + (target_x - S_X - PILL_CENTER_X) * step / 12))" \
            "$((S_Y + S_TOP / 2 + (target_y - S_Y - S_TOP / 2) * step / 12))"
        sleep 0.04
    done
    sleep 0.3
}

drag_to_target() {
    drag_hold_to_target "$1" "$2"
    xdotool mouseup 1
    sleep 1
}

# Give the first window two regions, then create a sibling process-window. The
# sibling remains a real GPUI client in the same process, which is exactly the
# enumeration the palette and X11 pointer route use.
SOURCE_WID=$(windows | head -1)
[ -n "$SOURCE_WID" ] || fail "initial Scribe window never mapped"
focus_window "$SOURCE_WID"
oracle wait-leaves 1 1 >/dev/null || fail "initial workspace never became live"
send_keys ctrl+alt+backslash
read -r SOURCE_A SOURCE_B <<<"$(oracle wait-leaves 2 1)" || fail "source split never became live"
send_keys ctrl+shift+n
for _ in $(seq 1 60); do
    mapfile -t mapped < <(windows)
    [ "${#mapped[@]}" -eq 2 ] && break
    sleep 0.3
done
[ "${#mapped[@]}" -eq 2 ] || fail "new sibling window never mapped"
TARGET_WID=""
for wid in "${mapped[@]}"; do [ "$wid" != "$SOURCE_WID" ] && TARGET_WID=$wid; done
[ -n "$TARGET_WID" ] || fail "could not identify sibling window"
# Keep both rectangles disjoint on the standard 1920x1080 Xvfb desktop.
xdotool windowsize "$SOURCE_WID" 800 600
xdotool windowmove "$SOURCE_WID" 80 180
xdotool windowsize "$TARGET_WID" 800 600
xdotool windowmove "$TARGET_WID" 1040 180
sleep 1
remember_source_geometry
remember_target_geometry

# Palette edge and centre rows must reach the same cross-window transaction.
# Their exact centre sentence and edge-detail absence are frozen by the shared
# `workspace_move_entries`/`swap_hint_text` unit oracle; this run proves those
# rows are reachable and routed through the real command palette.
reset_record
palette "Move workspace right of workspace"
assert_move "$SOURCE_B" right
echo "PHASE 1 PASS: palette edge move reached the sibling window"

# A centre swap cannot empty a source shell, so restore a second source region
# before exercising the palette's swap row.
focus_window "$SOURCE_WID"
send_keys ctrl+alt+backslash
read -r SOURCE_A SOURCE_SWAP <<<"$(oracle wait-leaves 2 1)" || fail "source split for palette swap never became live"
reset_record
palette "Swap workspace with workspace"
assert_move "$SOURCE_SWAP" swap
echo "PHASE 2 PASS: palette centre swap reached the sibling window"

# Rebuild a two-region source after the palette operations so pointer cancel,
# blur, edge insertion, and centre swap all start with a non-sole source.
focus_window "$SOURCE_WID"
send_keys ctrl+alt+backslash
read -r POINTER_A POINTER_B POINTER_C <<<"$(oracle wait-leaves 3 1)" || fail "source split for pointer path never became live"
remember_source_geometry
remember_target_geometry
TARGET_CENTER_X=$((T_X + T_W / 2))
TARGET_CENTER_Y=$((T_Y + T_TOP + T_GRID_H / 2))

# Escape and blur both clear cross-window previews before release. A committed
# move here would be visible on the wire, so no frame is a stronger oracle than
# a screenshot of a fading overlay.
reset_record
drag_hold_to_target "$TARGET_CENTER_X" "$TARGET_CENTER_Y"
send_keys Escape
xdotool mouseup 1
sleep 0.5
[ "$(oracle count MoveWorkspace)" -eq 0 ] || fail "Escape committed a cross-window move"

reset_record
drag_hold_to_target "$TARGET_CENTER_X" "$TARGET_CENTER_Y"
xdotool windowactivate --sync "$TARGET_WID" 2>/dev/null || true
xdotool mouseup 1
sleep 0.5
[ "$(oracle count MoveWorkspace)" -eq 0 ] || fail "blur committed a cross-window move"
echo "PHASE 3 PASS: Escape and blur clear sibling previews without a move"

# X11 pointer edge insert. The destination geometry is measured from its own
# painted chrome, never inferred from the source frame.
reset_record
focus_window "$SOURCE_WID"
drag_to_target "$((T_X + 18))" "$TARGET_CENTER_Y"
assert_move - left
echo "PHASE 4 PASS: measured X11 pointer edge insert reached the sibling"

# The edge leaves one source region. Split it again, then centre-drop the new
# region so the pointer path covers the same swap operation and shared wording
# as the palette route.
focus_window "$SOURCE_WID"
send_keys ctrl+alt+backslash
read -r POINTER_AFTER_EDGE POINTER_NEXT POINTER_NEW <<<"$(oracle wait-leaves 3 1)" || fail "source split for pointer swap never became live"
remember_source_geometry
remember_target_geometry
TARGET_CENTER_X=$((T_X + T_W / 2))
TARGET_CENTER_Y=$((T_Y + T_TOP + T_GRID_H / 2))
reset_record
drag_to_target "$TARGET_CENTER_X" "$TARGET_CENTER_Y"
assert_move - swap
echo "PHASE 5 PASS: measured X11 pointer centre swap reached the sibling"

echo "PASS: cross-window palette and X11 workspace moves"
