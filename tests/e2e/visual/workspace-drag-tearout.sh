#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 420
set -euo pipefail

. /tests/visual/tab-geometry-common.bash

# @lat: [[test#GPUI Workspace Drag]]
# User-reachable X11 oracle for the dedicated workspace-pill drag. Tree and wire
# state are authoritative; screenshots prove the real overlay/pill surfaces.
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
PILL_CENTER_X=44 # measured from 00-standalone-pill.png: 88px neutral label + padding
PLACEMENT_TOLERANCE=50

fail() {
    echo "FAIL: $1" >&2
    echo "--- tree/frame oracle ---" >&2
    oracle summary 2>/dev/null || true
    echo "--- client log ---" >&2
    tail -80 "$CLIENT_LOG" >&2 || true
    echo "--- server log ---" >&2
    tail -40 "$SERVER_LOG" >&2 || true
    exit 1
}

find_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

focus_window() {
    WID=${1:-$(find_windows | head -1)}
    [ -n "$WID" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.6
    local info
    info=$(xwininfo -id "$WID")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/Width:/ { print $2 }')
    WIN_H=$(printf '%s\n' "$info" | awk '/Height:/ { print $2 }')
}

shot() {
    scrot -o /output/workspace-drag-full.png
    convert /output/workspace-drag-full.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "/output/$1"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

click_rel() {
    xdotool mousemove --sync "$((WIN_X + $1))" "$((WIN_Y + $2))"
    xdotool click 1
    sleep 0.6
}

drag_rel() {
    local from_x=$1 from_y=$2 to_x=$3 to_y=$4 step
    xdotool mousemove --sync "$((WIN_X + from_x))" "$((WIN_Y + from_y))"
    xdotool mousedown 1
    xdotool mousemove_relative --sync -- 3 0
    for step in $(seq 1 12); do
        xdotool mousemove --sync \
            "$((WIN_X + from_x + (to_x - from_x) * step / 12))" \
            "$((WIN_Y + from_y + (to_y - from_y) * step / 12))"
        sleep 0.04
    done
    xdotool mouseup 1
    sleep 0.8
}

drag_hold_rel() {
    local from_x=$1 from_y=$2 to_x=$3 to_y=$4 step
    xdotool mousemove --sync "$((WIN_X + from_x))" "$((WIN_Y + from_y))"
    xdotool mousedown 1
    xdotool mousemove_relative --sync -- 3 0
    for step in $(seq 1 12); do
        xdotool mousemove --sync \
            "$((WIN_X + from_x + (to_x - from_x) * step / 12))" \
            "$((WIN_Y + from_y + (to_y - from_y) * step / 12))"
        sleep 0.04
    done
    sleep 0.3
}

cancel_hold() {
    xdotool keydown Escape
    sleep 0.1
    xdotool keyup Escape
    xdotool mouseup 1
    sleep 0.6
}

cancel_hold_on_source() {
    xdotool mousemove --sync "$((WIN_X + WIN_W / 4))" \
        "$((WIN_Y + GRID_TOP + GRID_H / 2))"
    xdotool mouseup 1
    sleep 0.6
}

reset_record() {
    : >"$RECORD"
    sleep 0.3
}

ORACLE=/tests/visual/workspace-tree-oracle.py
oracle() { python3 "$ORACLE" "$RECORD" "$@"; }

wait_for_tree_change() {
    local before=$1 deadline=$((SECONDS + 25)) current
    while [ "$SECONDS" -lt "$deadline" ]; do
        current=$(oracle summary)
        [ "$current" != "$before" ] && { printf '%s' "$current"; return 0; }
        sleep 0.2
    done
    return 1
}

assert_zero_gesture_frames() {
    local counts=$1
    python3 - "$counts" <<'PY'
import json,sys
counts=json.loads(sys.argv[1])
for name in ("ReportWorkspaceTree","FocusChanged","KeyInput"):
    assert counts.get(name,0)==0,(name,counts)
PY
}

# An 18%-alpha zone changes nearly every pixel under its code-derived rect.
# Baseline rows must change every pixel; other overlay probes retain their
# broader threshold. Crop dimensions mirror zone_preview_rect's thirds.
assert_overlay_rect() {
    local before=$1 after=$2 x=$3 y=$4 w=$5 h=$6 label=$7 baseline=${8:-0} value area
    value=$(compare -metric AE \
        \( "$before" -crop "${w}x${h}+${x}+${y}" +repage \) \
        \( "$after" -crop "${w}x${h}+${x}+${y}" +repage \) null: 2>&1 || true)
    value=${value%%.*}
    area=$((w * h))
    if [ "$baseline" -eq 1 ]; then
        [ "${value:-0}" -eq "$area" ] \
            || fail "$label overlay changed ${value:-0}/$area expected pixels"
        printf '{"label":"%s","x":%d,"y":%d,"width":%d,"height":%d,"changed":%d}\n' \
            "$label" "$x" "$y" "$w" "$h" "$value" >>/output/workspace-zone-baselines.jsonl
    else
        [ "${value:-0}" -gt $((area / 2)) ] \
            || fail "$label overlay changed ${value:-0}/$area expected pixels"
    fi
}

focus_window
for _ in $(seq 1 80); do
    if ids=$(oracle wait-leaves 1 1 2>/dev/null); then break; fi
    sleep 0.2
done
[ -n "${ids:-}" ] || fail "initial workspace tree never became live"

# Split into two side-by-side regions. The relay holds SessionCreated for the
# configured delay, making the real zero-tab standalone pill observable.
reset_record
send_keys ctrl+alt+backslash
ids=$(oracle wait-leaves 2 0) || fail "workspace split never reported two leaves"
read -r a b <<<"$ids"
[ "$(oracle leaf-session-count "$b")" -eq 0 ] \
    || fail "standalone workspace received a flex tab before chrome move"
shot 00-standalone-pill.png
GRID_TOP=$(measure_bar_height /output/00-standalone-pill.png 0 100 workspace-top)
STATUS_H=$(measure_status_height \
    /output/00-standalone-pill.png "$(( WIN_H - 100 ))" 100 workspace-status)
GRID_BOTTOM=$(( WIN_H - STATUS_H ))
GRID_H=$(( GRID_BOTTOM - GRID_TOP ))
[ "$GRID_H" -gt 0 ] || fail "measured grid height $GRID_H is not positive"
right_ink=$(convert /output/00-standalone-pill.png \
    -crop "88x${GRID_TOP}+$((WIN_W / 2))+0" +repage \
    -colorspace Gray -threshold 35% -format '%[fx:mean*w*h]' info:)
right_ink=${right_ink%.*}
[ "${right_ink:-0}" -ge 20 ] || fail "zero-tab standalone pill has no visible label ($right_ink px)"
echo "PHASE 0 PASS: neutral standalone pill appeared before its delayed tab"

# Empty titlebar chrome still belongs to compositor window move, not the new
# pill drag. The pill's measured center fixes its 88px span; capture and guard
# an ink-free post-pill target patch before pressing it.
title_y=$((GRID_TOP / 2))
empty_x=$((WIN_W / 2 + PILL_CENTER_X * 2 + 12))
[ "$empty_x" -lt "$WIN_W" ] || fail "zero-tab pill leaves no blank titlebar target"
blank_target=/output/00-standalone-blank-target.png
convert /output/00-standalone-pill.png \
    -crop "9x9+$((empty_x - 4))+$((title_y - 4))" +repage "$blank_target"
blank_ink=$(convert "$blank_target" -colorspace Gray -threshold 35% \
    -format '%[fx:mean*w*h]' info:)
blank_ink=${blank_ink%.*}
blank_ink_w=$(band_ink_width "$blank_target")
blank_ink_w=${blank_ink_w%.*}
[ "${blank_ink:-0}" -eq 0 ] && [ "${blank_ink_w:-0}" -le 1 ] \
    || fail "zero-tab blank titlebar target has ${blank_ink:-0}px painted ink"
old_x=$WIN_X; old_y=$WIN_Y
xdotool mousemove --sync "$((WIN_X + empty_x))" "$((WIN_Y + title_y))"
xdotool mousedown 1
sleep 0.1
# Cross the titlebar's 4px threshold while still inside its measured band;
# only then leave it, after the compositor has accepted the native move request.
xdotool mousemove --sync "$((WIN_X + empty_x + 8))" "$((WIN_Y + title_y))"
sleep 0.1
xdotool mousemove --sync "$((WIN_X + empty_x + 80))" "$((WIN_Y + title_y + 50))"
xdotool mouseup 1
sleep 0.2
info=$(xwininfo -id "$WID")
new_x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
new_y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
[ "$new_x" -ne "$old_x" ] || [ "$new_y" -ne "$old_y" ] \
    || fail "dragging zero-tab empty chrome did not move the window"
# Every later target and screenshot uses the original origin, so restore it
# after proving the native move rather than silently shifting their surface.
xdotool windowmove --sync "$WID" "$old_x" "$old_y"
focus_window "$WID"
xdotool windowmove --sync "$WID" \
    "$((2 * old_x - WIN_X))" "$((2 * old_y - WIN_Y))"
focus_window "$WID"
[ "$WIN_X" -eq "$old_x" ] && [ "$WIN_Y" -eq "$old_y" ] \
    || fail "zero-tab empty chrome did not restore window origin ($WIN_X,$WIN_Y)"
echo "PHASE 6 PASS: zero-tab empty chrome moved and restored the window ($old_x,$old_y -> $new_x,$new_y)"

ids=$(oracle wait-leaves 2 1) || fail "split workspaces never received tabs"
read -r a b <<<"$ids"
# Let the delayed attach/replay settle before zero-diff gesture windows start.
sleep 2
focus_window "$WID"

pill_left=$PILL_CENTER_X
pill_right=$((WIN_W / 2 + PILL_CENTER_X))
left_center=$((WIN_W / 4))
right_center=$((3 * WIN_W / 4))
grid_mid=$((GRID_TOP + GRID_H / 2))
target_x=$((WIN_W / 2))
target_w=$((WIN_W / 2))
third_w=$((target_w / 3))
third_h=$((GRID_H / 3))

# Paint every zone against the exact rect production computes. Escape retires
# each probe without a tree/focus/PTY diff.
: >/output/workspace-zone-baselines.jsonl
for zone in left right top bottom center; do
    shot "zone-$zone-before.png"
    case "$zone" in
        left)   px=$((target_x + 18)); py=$grid_mid; ox=$target_x; oy=$GRID_TOP; ow=$third_w; oh=$GRID_H ;;
        right)  px=$((WIN_W - 18)); py=$grid_mid; ox=$((target_x + target_w - third_w)); oy=$GRID_TOP; ow=$third_w; oh=$GRID_H ;;
        top)    px=$right_center; py=$((GRID_TOP + 18)); ox=$target_x; oy=$GRID_TOP; ow=$target_w; oh=$third_h ;;
        bottom) px=$right_center; py=$((GRID_BOTTOM - 18)); ox=$target_x; oy=$((GRID_BOTTOM - third_h)); ow=$target_w; oh=$third_h ;;
        center) px=$right_center; py=$grid_mid; ox=$((target_x + third_w)); oy=$((GRID_TOP + third_h)); ow=$third_w; oh=$third_h ;;
    esac
    reset_record
    drag_hold_rel "$pill_left" "$title_y" "$px" "$py"
    shot "zone-$zone-held.png"
    assert_overlay_rect "/output/zone-$zone-before.png" "/output/zone-$zone-held.png" \
        "$ox" "$oy" "$ow" "$oh" "$zone" 1
    cancel_hold_on_source
    assert_zero_gesture_frames "$(oracle counts)"
done
echo "PHASE 1 PASS: all five zone overlays match code-derived thirds"

# Escape must be claimed while the pill drag is live. Follow with another live
# preview to prove cancellation clears only this gesture, not the next one.
reset_record
drag_hold_rel "$pill_left" "$title_y" "$right_center" "$grid_mid"
cancel_hold
assert_zero_gesture_frames "$(oracle counts)"
shot escape-next-before.png
reset_record
drag_hold_rel "$pill_left" "$title_y" "$right_center" "$grid_mid"
shot escape-next-held.png
assert_overlay_rect /output/escape-next-before.png /output/escape-next-held.png \
    "$((target_x + third_w))" "$((GRID_TOP + third_h))" "$third_w" "$third_h" escape-next
cancel_hold_on_source
assert_zero_gesture_frames "$(oracle counts)"
echo "PHASE 2 PASS: Escape was zero-diff and the next drag armed normally"

# A plain click on the first titlebar pill keeps its old focus behavior: the
# next tab creation must land in that exact workspace.
click_rel "$pill_left" "$title_y"
left_tabs_before=$(oracle leaf-session-count "$a")
send_keys ctrl+shift+t
for _ in $(seq 1 60); do
    left_tabs_after=$(oracle leaf-session-count "$a")
    [ "$left_tabs_after" -gt "$left_tabs_before" ] && break
    sleep 0.3
done
[ "${left_tabs_after:-0}" -gt "$left_tabs_before" ] \
    || fail "plain pill click did not focus workspace $a"
echo "PHASE 3 PASS: pill click focused its region without becoming a drag"

# Corner ties must resolve horizontally. Convert a 12px horizontal inset to
# the equal normalized vertical inset for this measured target aspect ratio.
corner_dy=$(((12 * GRID_H + target_w - 1) / target_w))
for corner in top-left bottom-left top-right bottom-right; do
    case "$corner" in
        top-left)
            source_pill=$pill_left; px=$((target_x + 12)); py=$((GRID_TOP + corner_dy))
            ox=$target_x; cancel_x=$left_center ;;
        bottom-left)
            source_pill=$pill_left; px=$((target_x + 12)); py=$((GRID_BOTTOM - corner_dy))
            ox=$target_x; cancel_x=$left_center ;;
        top-right)
            source_pill=$pill_right; px=$((target_x - 12)); py=$((GRID_TOP + corner_dy))
            ox=$((target_x - third_w)); cancel_x=$right_center ;;
        bottom-right)
            source_pill=$pill_right; px=$((target_x - 12)); py=$((GRID_BOTTOM - corner_dy))
            ox=$((target_x - third_w)); cancel_x=$right_center ;;
    esac
    shot "corner-$corner-before.png"
    reset_record
    drag_hold_rel "$source_pill" "$title_y" "$px" "$py"
    shot "corner-$corner-held.png"
    assert_overlay_rect "/output/corner-$corner-before.png" "/output/corner-$corner-held.png" \
        "$ox" "$GRID_TOP" "$third_w" "$GRID_H" "$corner-horizontal-tie"
    xdotool mousemove --sync "$((WIN_X + cancel_x))" "$((WIN_Y + grid_mid))"
    xdotool mouseup 1
    sleep 0.6
    assert_zero_gesture_frames "$(oracle counts)"
done
echo "PHASE 4 PASS: all four deterministic corner ties chose horizontal zones"

# Commit swap and all four structural edges. Trees, not pixels, distinguish
# left/right and top/bottom outcomes.
reset_record
drag_rel "$pill_left" "$title_y" "$right_center" "$grid_mid"
oracle wait-root Horizontal "$b" "$a" >/output/workspace-swap-tree.json \
    || fail "center swap did not exchange leaves"
[ "$(oracle counts | python3 -c 'import json,sys; print(json.load(sys.stdin).get("KeyInput",0))')" -eq 0 ] \
    || fail "workspace swap leaked PTY input"

reset_record
drag_rel "$pill_right" "$title_y" 24 "$grid_mid"
oracle wait-root Horizontal "$a" "$b" >/output/workspace-left-tree.json \
    || fail "left edge insert did not produce [A,B]"
reset_record
drag_rel "$pill_left" "$title_y" "$((WIN_W - 24))" "$grid_mid"
oracle wait-root Horizontal "$b" "$a" >/output/workspace-right-tree.json \
    || fail "right edge insert did not produce [B,A]"
reset_record
drag_rel "$pill_right" "$title_y" "$left_center" "$((GRID_TOP + 18))"
oracle wait-root Vertical "$a" "$b" >/output/workspace-top-tree.json \
    || fail "top edge insert did not produce vertical [A,B]"
reset_record
drag_rel "$pill_left" "$title_y" "$((WIN_W / 2))" "$((GRID_BOTTOM - 18))"
oracle wait-root Vertical "$b" "$a" >/output/workspace-bottom-tree.json \
    || fail "bottom edge insert did not produce vertical [B,A]"
echo "PHASE 5 PASS: swap and four edge inserts committed exact wire trees"

# Source-disappearance cleanup: a third workspace's shell exits while its pill
# owns a live drag. The region collapses, release leaves no stuck overlay, and a
# fresh split/drag works immediately afterwards.
click_rel "$((WIN_W / 4))" "$((GRID_TOP + GRID_H / 4))"
send_keys ctrl+alt+backslash
ids=$(oracle wait-leaves 3 1) || fail "third workspace did not become live"
click_rel "$((3 * WIN_W / 4))" "$((GRID_TOP + GRID_H / 4))"
xdotool type --clearmodifiers --delay 2 'sleep 2; exit'
xdotool key --clearmodifiers Return
sleep 0.2
third_pill=$((WIN_W / 2 + PILL_CENTER_X))
drag_hold_rel "$third_pill" "$title_y" "$((WIN_W / 4))" "$((GRID_TOP + GRID_H / 4))"
for _ in $(seq 1 80); do
    if ids=$(oracle wait-leaves 2 1 2>/dev/null); then break; fi
    sleep 0.2
done
xdotool mouseup 1
[ -n "${ids:-}" ] || fail "source workspace did not disappear"
# A completed drag immediately afterwards is the user-visible cleanup proof;
# creating another session would mix its asynchronous bootstrap with this race.
reset_record
drag_rel "$PILL_CENTER_X" "$title_y" "$((WIN_W / 2))" \
    "$((GRID_TOP + 3 * GRID_H / 4))"
for _ in $(seq 1 50); do
    reports=$(oracle counts | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ReportWorkspaceTree",0))')
    [ "$reports" -gt 0 ] && break
    sleep 0.2
done
[ "${reports:-0}" -gt 0 ] || fail "next workspace drag did not report after disappearance"
echo "PHASE 7 PASS: source disappearance cleaned the drag and the next gesture worked"

# The titlebar pill belongs to the first leaf. Focus that exact region before
# enriching it, then retain its id for the pointer tear-out source and tree
# oracle instead of assuming the initial workspace still occupies that slot.
ids=$(oracle wait-leaves 2 1) || fail "two-region fixture vanished before tear-out"
read -r source_workspace other_workspace <<<"$ids"
click_rel "$PILL_CENTER_X" "$title_y"
# Pill selection leaves keyboard focus in titlebar chrome; return it to the
# first region before issuing pane/tab chords.
click_rel "$((WIN_W / 4))" "$((GRID_TOP + GRID_H / 4))"
send_keys ctrl+shift+t
pane_adopts_before=$(grep -acF 'pane adopted a session' "$CLIENT_LOG" 2>/dev/null || true)
send_keys ctrl+shift+backslash
for _ in $(seq 1 60); do
    pane_adopts_after=$(grep -acF 'pane adopted a session' "$CLIENT_LOG" 2>/dev/null || true)
    [ "$pane_adopts_after" -gt "$pane_adopts_before" ] && break
    sleep 0.2
done
[ "${pane_adopts_after:-0}" -gt "$pane_adopts_before" ] \
    || fail "pane split never adopted a session before tear-out"
# The tap deliberately delays each SessionCreated to expose standalone pills;
# a pane split can therefore still be adopting its sibling after the first
# pane arrives. Wait out both relay slots before snapshotting the exact leaf.
sleep 3
program_cmd="sh -c 'echo \$\$ > /tmp/workspace-drag-program.pid; exec sleep 300' & echo WORKSPACE_DRAG_PROGRAM_ALIVE"
xdotool type --clearmodifiers --delay 1 "$program_cmd"
xdotool key --clearmodifiers Return
for _ in $(seq 1 60); do [ -s /tmp/workspace-drag-program.pid ] && break; sleep 0.2; done
[ -s /tmp/workspace-drag-program.pid ] || fail "long-lived program did not start"
kill -0 "$(cat /tmp/workspace-drag-program.pid)" || fail "long-lived program already exited"
source_leaf=/output/workspace-tear-source-leaf.json
last_leaf=''
stable_leaf_polls=0
for _ in $(seq 1 40); do
    current_leaf=$(oracle leaf "$source_workspace" 2>/dev/null || true)
    [ -n "$current_leaf" ] || { sleep 0.2; continue; }
    if [ "$current_leaf" = "$last_leaf" ]; then
        stable_leaf_polls=$((stable_leaf_polls + 1))
    else
        last_leaf=$current_leaf
        stable_leaf_polls=0
    fi
    [ "$stable_leaf_polls" -ge 4 ] && break
    sleep 0.2
done
[ "$stable_leaf_polls" -ge 4 ] || fail "source leaf did not settle before tear-out"
printf '%s\n' "$last_leaf" >"$source_leaf"
python3 - "$source_leaf" <<'PY'
import json,sys
leaf=json.load(open(sys.argv[1]))
assert len(leaf.get("session_ids",[])) >= 2, leaf
assert any(tree is not None for tree in leaf.get("pane_trees",[])), leaf
assert 0 <= leaf.get("active_tab_index",0) < len(leaf["session_ids"]), leaf
PY

# Arm in the universal 8px band and release at a known global point. X11 should
# request the new top-left there; openbox decorations may shift it within the
# documented tolerance.
focus_window "$WID"
release_global_x=$((WIN_X + 4))
release_global_y=$((WIN_Y + title_y))
reset_record
drag_rel "$PILL_CENTER_X" "$title_y" 4 "$title_y"
for _ in $(seq 1 100); do
    mapfile -t windows < <(find_windows)
    [ "${#windows[@]}" -eq 2 ] && break
    sleep 0.3
done
[ "${#windows[@]}" -eq 2 ] || fail "tear-out did not map a second window"
new_wid=""
for candidate in "${windows[@]}"; do
    [ "$candidate" != "$WID" ] && new_wid=$candidate
done
[ -n "$new_wid" ] || fail "could not identify torn-out window"
transfer_json=$(oracle assert-transfer "$source_workspace" "$source_leaf" "$other_workspace") \
    || fail "tear-out did not preserve exact source/target trees"
kill -0 "$(cat /tmp/workspace-drag-program.pid)" \
    || fail "workspace program died during tear-out"
info=$(xwininfo -id "$new_wid")
new_x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
new_y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
dx=$((new_x - release_global_x)); [ "$dx" -lt 0 ] && dx=$((-dx))
dy=$((new_y - release_global_y)); [ "$dy" -lt 0 ] && dy=$((-dy))
[ "$dx" -le "$PLACEMENT_TOLERANCE" ] && [ "$dy" -le "$PLACEMENT_TOLERANCE" ] \
    || fail "X11 tear-out landed $dx,$dy px from release (tolerance $PLACEMENT_TOLERANCE)"
shot 09-tearout.png
printf '%s\n' "$transfer_json" >/output/workspace-tearout-oracle.json
echo "PHASE 8 PASS: tear-out preserved ids/tab/pane tree/program and landed within $PLACEMENT_TOLERANCE px"

echo "PASS: workspace drag and tear-out pointer matrix"
