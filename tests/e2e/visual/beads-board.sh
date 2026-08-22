#!/bin/bash
# e2e-timeout: 420
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: Docker E2E only" >&2
    exit 99
}
set -euo pipefail

CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
CONTRACT="${SCRIBE_A2A3_CONTRACT:-/contract/a2a3-contract.json}"
IMAGE_ORACLE=/tests/beads_board_image_oracle.py
ORACLE=/tests/visual/beads-board-oracle.py

fail() {
    echo "FAIL: $1" >&2
    tail -60 "$CLIENT_LOG" 2>/dev/null || true
    exit 1
}

window_id() {
    xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1
}

focus() {
    local wid
    wid=$(window_id)
    [ -n "$wid" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$wid" 2>/dev/null || xdotool windowfocus --sync "$wid"
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_W=$WIDTH
    WIN_H=$HEIGHT
}

shot() {
    import -window "$WID" "/output/$1"
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep "${2:-0.28}"
}

first_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys
found = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if row.get("dir") == "client" and message.get("type") == "CreateSession":
        found = message["workspace_id"]
if not found:
    raise SystemExit(1)
print(found)
PY
}

other_workspace() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
found = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "WorkspaceInfo"
            and message.get("workspace_id") != sys.argv[2]):
        found = message["workspace_id"]
if not found:
    raise SystemExit(1)
print(found)
PY
}

latest_rows() {
    python3 - "$CLIENT_LOG" "${1:-}" <<'PY'
import re, sys
ansi = re.compile(r'\x1b\[[0-9;]*m')
want = sys.argv[2]
if want and not want.startswith("ws-"):
    want = f"ws-{want[:8]}"
rows = []
for line in open(sys.argv[1], errors="replace"):
    line = ansi.sub("", line)
    if "published a pane's grid size" not in line:
        continue
    if want and f"workspace_id={want}" not in line:
        continue
    match = re.search(r"rows=(\d+)", line)
    if match:
        rows.append(int(match.group(1)))
print(rows[-1] if rows else 0)
PY
}

latest_session() {
    python3 - "$RECORD" <<'PY'
import json, sys
found = ""
for line in open(sys.argv[1]):
    try:
        message = json.loads(line).get("message", {})
    except ValueError:
        continue
    if message.get("session_id"):
        found = message["session_id"]
print(found)
PY
}

board_message() {
    python3 - "$1" "$2" <<'PY'
import json, sys, time
workspace, fixture = sys.argv[1:]
updated = "2026-08-20T00:00:00Z"

def item(issue_id, title, priority, epic=None, blockers=()):
    row = {
        "id": issue_id,
        "title": title,
        "priority": priority,
        "blocker_ids": list(blockers),
        "parent_epic_name": epic[1] if epic else None,
        "updated_at": updated,
    }
    if epic:
        row["parent_epic_id"] = epic[0]
    return row

if fixture == "empty":
    queues = {name: [] for name in ("backlog", "ready", "in_progress", "blocked", "done")}
    totals = {name: 0 for name in queues}
elif fixture == "sparse":
    shared = ("pi-epic", "Pi AI integration")
    queues = {
        "backlog": [],
        "ready": [],
        "in_progress": [
            item("ip-1", "Build the Pi lifecycle extension", 1, shared),
            item("ip-2", "Promote Pi launch and restore", 1, shared),
            item("ip-3", "Prove Pi integration end to end", 2, shared),
            item("ip-4", "Document Pi compatibility", 2, shared),
        ],
        "blocked": [
            item("bl-1", "Verify shared AI behavior", 1, shared, ("ip-1",)),
            item("bl-2", "Install and package extension", 1, shared, ("ip-1",)),
            item("bl-3", "Document integration", 2, shared, ("ip-2",)),
            item("bl-4", "Run packaged acceptance", 2, shared, ("ip-2",)),
        ],
        "done": [
            item("dn-1", "Negotiate provider compatibility", 0),
            item("dn-2", "Add provider protocol", 1),
            item("dn-3", "Land lifecycle state", 2),
            item("dn-4", "Wire agent hooks", 3),
            item("dn-5", "Document old path", 4),
        ],
    }
    totals = {"backlog": 0, "ready": 0, "in_progress": 4, "blocked": 4, "done": 559}
else:
    flow_epic = ("flow-epic", "Flow epic")
    if fixture == "deep":
        flow_epic = ("deep-epic", "Deep flow epic")
    queues = {
        "backlog": [
            item("bg-0", "P0 backlog contract", 0),
            item("bg-1", "P1 backlog contract", 1, ("mixed-a", "Mixed epic A")),
            item("bg-2", "P2 backlog contract", 2),
            item("bg-3", "P3 backlog contract", 3),
            item("bg-4", "P4 backlog contract", 4),
        ],
        "ready": [
            item("flow-open", "Open the Flow contract", 3, flow_epic),
            item("rd-4", "P4 metadata alignment", 4, ("mixed-b", "Mixed epic B")),
            item("rd-0", "P0 ready overflow", 0),
            item("rd-1", "P1 hidden row", 1),
            item("rd-2", "P2 hidden row", 2),
        ],
        "in_progress": [
            item("ip-0", "Progress state and age", 2, ("mixed-c", "Mixed epic C")),
            item("ip-1", "Priority and title alignment", 1),
            item("ip-2", "Overflow chevron contract", 3, ("mixed-d", "Mixed epic D")),
            item("ip-3", "Hidden progress row", 4),
            item("ip-4", "Second hidden progress row", 0),
        ],
        "blocked": [
            item("bl-0", "Blocked drawer row", 1, ("mixed-a", "Mixed epic A"), ("bg-0",)),
            item("bl-1", "Blocked metadata row", 2, ("mixed-b", "Mixed epic B"), ("rd-4",)),
            item("bl-2", "Blocked overflow row", 3, blockers=("ip-0",)),
            item("bl-3", "Hidden blocked row", 4, blockers=("ip-1",)),
        ],
        "done": [
            item("dn-0", "Done drawer row", 1),
            item("dn-1", "Done metadata row", 2, ("mixed-c", "Mixed epic C")),
            item("dn-2", "Done overflow row", 3),
            item("dn-3", "Hidden done row", 4),
        ],
    }
    totals = {"backlog": 12, "ready": 5, "in_progress": 6, "blocked": 4, "done": 38}

snapshot = {
    "refreshed_at_epoch_ms": int(time.time() * 1000),
    **queues,
    **{f"{name}_total": value for name, value in totals.items()},
}
print(json.dumps({
    "type": "BeadsBoard",
    "workspace_id": workspace,
    "protocol_version": 1,
    "state": {"Ready": {"snapshot": snapshot, "stale": False, "refresh_error": None}},
}, separators=(",", ":")))
PY
}

# ponytail: periodic last-good injection only counters the bd-less visual
# server's NotDetected poll; replace it if the harness gains a static-board mode.
KEEPER_FILE=/tmp/beads-board-keeper.json
KEEPER_PID=""

set_board() {
    local fixture=$1 next="${KEEPER_FILE}.next"
    board_message "$WORKSPACE" "$fixture" >"$next"
    mv "$next" "$KEEPER_FILE"
}

start_board_keeper() {
    (
        while true; do
            if [ -s "$KEEPER_FILE" ]; then
                scribe-test share-inject --control "$CONTROL" "$(cat "$KEEPER_FILE")" >/dev/null 2>&1 || true
            fi
            sleep 0.12
        done
    ) &
    KEEPER_PID=$!
}

stop_board_keeper() {
    if [ -n "$KEEPER_PID" ]; then
        kill "$KEEPER_PID" 2>/dev/null || true
        wait "$KEEPER_PID" 2>/dev/null || true
        KEEPER_PID=""
    fi
}

flow_graph() {
    python3 - "$1" "$2" <<'PY'
import json, sys
workspace, fixture = sys.argv[1:]

def node(issue_id, title, priority, status, queue, assignee=None):
    return {
        "id": issue_id,
        "title": title,
        "priority": priority,
        "status": status,
        "queue": queue,
        "assignee": assignee,
        "updated_at": "2026-08-20T00:00:00Z",
    }

if fixture == "standard":
    epic_id = "flow-epic"
    graph = {
        "epic_id": epic_id,
        "epic_title": "Flow epic",
        "closed": 2,
        "total": 7,
        "nodes": [
            node("fl-a", "Alpha foundation", 0, "closed", "done"),
            node("fl-b", "Bravo blocked", 1, "open", "blocked"),
            node("fl-c", "Charlie ready", 2, "open", "ready", "codex-agent-run"),
            node("flow-open", "Open the Flow contract", 3, "open", "in_progress"),
            node("fl-e", "Echo done", 4, "closed", "done"),
            node("fl-f", "Foxtrot ready", 2, "open", "ready"),
            node("fl-g", "Golf backlog", 4, "open", "backlog"),
        ],
        "edges": [
            {"from": "fl-a", "to": "fl-b"},
            {"from": "fl-a", "to": "fl-c"},
            {"from": "fl-b", "to": "flow-open"},
            {"from": "fl-c", "to": "fl-e"},
            {"from": "flow-open", "to": "fl-f"},
            {"from": "fl-e", "to": "fl-f"},
            {"from": "fl-f", "to": "fl-g"},
            {"from": "fl-a", "to": "fl-f"},
        ],
    }
else:
    epic_id = "deep-epic"
    roots = [
        node("deep-a", "Deep frontier progress", 1, "open", "in_progress"),
        node("deep-b", "Deep frontier ready", 1, "open", "ready"),
        node("deep-c", "Deep frontier blocked", 2, "open", "blocked"),
        node("deep-d", "Deep frontier backlog", 3, "open", "backlog"),
    ]
    chain = [
        node("deep-1", "Rank one", 1, "open", "blocked"),
        node("deep-2", "Rank two", 2, "open", "blocked"),
        node("deep-3", "Rank three", 2, "open", "blocked"),
        node("deep-4", "Rank four", 2, "open", "blocked"),
        node("deep-5", "Rank five", 2, "open", "blocked"),
        node("flow-open", "Deep cursor rank", 2, "open", "in_progress"),
        node("deep-z", "Far frontier", 2, "open", "ready", "codex-agent-run"),
    ]
    graph = {
        "epic_id": epic_id,
        "epic_title": "Deep flow epic",
        "closed": 0,
        "total": len(roots) + len(chain),
        "nodes": roots + chain,
        "edges": [
            *({"from": root["id"], "to": "deep-1"} for root in roots),
            *({"from": chain[index]["id"], "to": chain[index + 1]["id"]} for index in range(len(chain) - 1)),
        ],
    }
print(json.dumps({
    "type": "BeadsEpicGraph",
    "workspace_id": workspace,
    "epic_id": epic_id,
    "outcome": {"graph": graph},
}, separators=(",", ":")))
PY
}

load_tracks() {
    local image=$1 entries
    read -r -a entries <<<"$(python3 "$ORACLE" tracks "$CONTRACT" "$image" "$BOARD_TOP")"
    [ "${#entries[@]}" -eq 5 ] || fail "track oracle returned ${#entries[@]} tracks"
    IFS=: read -r T0_X T0_W <<<"${entries[0]}"
    IFS=: read -r T1_X T1_W <<<"${entries[1]}"
    IFS=: read -r T3_X T3_W <<<"${entries[3]}"
    IFS=: read -r T4_X T4_W <<<"${entries[4]}"
}

park_pointer() {
    xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 60))"
    sleep "${1:-0.35}"
}

board_park() {
    xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((BOARD_TOP + A2_STRIP_H - 10))"
    sleep "${1:-0.25}"
}

open_badge() {
    local offset=${1:-0}
    xdotool mousemove --sync --window "$WID" "$((offset + 13))" 17
    sleep 0.45
}

show_board() {
    local fixture=$1
    set_board "$fixture"
    for _ in 1 2 3; do
        park_pointer 0.2
        open_badge
        inject "$(board_message "$WORKSPACE" "$fixture")" 0.05
        shot beads-show-probe.png
        if python3 "$ORACLE" tracks "$CONTRACT" /output/beads-show-probe.png "$BOARD_TOP" >/dev/null 2>&1; then
            return
        fi
    done
    fail "could not open $fixture board from its titlebar badge"
}

ensure_pinned_board() {
    local fixture=$1 rows
    set_board "$fixture"
    sleep 0.2
    rows=$(latest_rows "$WORKSPACE")
    if [ "$rows" -gt "$PINNED_ROWS" ]; then
        open_badge
        xdotool click 1
        sleep 0.25
    fi
    inject "$(board_message "$WORKSPACE" "$fixture")" 0.04
    park_pointer 0.08
}

enter_flow() {
    local fixture=$1 graph=$2
    show_board "$fixture"
    shot beads-flow-lanes-current.png
    load_tracks /output/beads-flow-lanes-current.png
    local x=$((T1_X + 70))
    local y=$((BOARD_TOP + A2_HEADBAND_H + 12))
    xdotool mousemove --sync --window "$WID" "$x" "$y"
    xdotool click 1
    scribe-test share-inject --control "$CONTROL" "$(flow_graph "$WORKSPACE" "$graph")"
    sleep 0.6
    xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((BOARD_TOP + A3_GRAPH_TOP + A3_GRAPH_H - 8))"
    sleep 0.2
}

wait_theme_reload() {
    local before
    before=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
    for _ in $(seq 1 40); do
        [ "$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)" -gt "$before" ] && return
        sleep 0.25
    done
    fail "client did not hot-reload appearance config"
}

strip_diff() {
    convert "$1" "$2" -compose difference -composite -colorspace Gray \
        -crop "${WIN_W}x${A2_STRIP_H}+0+${BOARD_TOP}" +repage -threshold 10% \
        -format "%[fx:round(mean*w*h)]" info:
}

[ -r "$CONTRACT" ] || fail "generated A2/A3 contract is not mounted"
[ "${SCRIBE_DISABLE_ANIMATIONS:-0}" = "1" ] || fail "visual contract must run with reduced motion"
eval "$(python3 "$IMAGE_ORACLE" contract-env "$CONTRACT")"

sleep 2
focus
WID=$(window_id)
xdotool windowsize --sync "$WID" "$A3_VIEWPORT_W" 739
sleep 0.6
focus
WORKSPACE=$(first_workspace) || fail "no workspace recorded"
set_board sparse
start_board_keeper
trap stop_board_keeper EXIT
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":null}"
inject "$(board_message "$WORKSPACE" sparse)"
park_pointer 0.2
shot a2-closed.png
open_badge
shot a2-open-probe.png
BOARD_TOP=$(python3 "$ORACLE" board-top "$CONTRACT" /output/a2-closed.png /output/a2-open-probe.png) || fail "could not measure A2 board top"
[ "$BOARD_TOP" -ge 0 ] || fail "invalid board top $BOARD_TOP"

# Pin the board reservation; lane pinning below is a separate A2 state.
UNPINNED_ROWS=$(latest_rows "$WORKSPACE")
xdotool click 1
for _ in $(seq 1 20); do
    PINNED_ROWS=$(latest_rows "$WORKSPACE")
    [ "$PINNED_ROWS" -lt "$UNPINNED_ROWS" ] && break
    sleep 0.15
done
[ "${PINNED_ROWS:-$UNPINNED_ROWS}" -lt "$UNPINNED_ROWS" ] || fail "board badge did not pin the strip"
inject "$(board_message "$WORKSPACE" sparse)"
park_pointer
shot a2-collapsed-sparse.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-collapsed-sparse.png "$BOARD_TOP" sparse --width "$WIN_W"
load_tracks /output/a2-collapsed-sparse.png

# Both collapsed tabs open the same exact overlay on their own hover repaint,
# then close after the board's existing grace with no unrelated input.
stop_board_keeper
sleep 0.2
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
sleep 0.35
shot a2-hover-blocked.png
python3 "$ORACLE" a2-drawer "$CONTRACT" /output/a2-collapsed-sparse.png /output/a2-hover-blocked.png "$BOARD_TOP"
park_pointer 0.45
shot a2-hover-blocked-closed.png
python3 "$ORACLE" a2-drawer-closed "$CONTRACT" /output/a2-collapsed-sparse.png /output/a2-hover-blocked-closed.png "$BOARD_TOP"
xdotool mousemove --sync --window "$WID" "$((T4_X + T4_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
sleep 0.35
shot a2-hover-done.png
python3 "$ORACLE" a2-drawer "$CONTRACT" /output/a2-collapsed-sparse.png /output/a2-hover-done.png "$BOARD_TOP"
park_pointer 0.45
shot a2-hover-done-closed.png
python3 "$ORACLE" a2-drawer-closed "$CONTRACT" /output/a2-collapsed-sparse.png /output/a2-hover-done-closed.png "$BOARD_TOP"
set_board sparse
start_board_keeper
sleep 0.15

# Pin each rail lane in turn; activation on the stable focused control unpins.
set_board busy
park_pointer 0.5
inject "$(board_message "$WORKSPACE" busy)" 0.08
shot a2-pin-done-base.png
load_tracks /output/a2-pin-done-base.png
xdotool mousemove --sync --window "$WID" "$((T4_X + T4_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
xdotool click 1
sleep 0.2
park_pointer 0.1
shot a2-pinned-done.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-pinned-done.png "$BOARD_TOP" pinned-done --width "$WIN_W"
load_tracks /output/a2-pinned-done.png
xdotool mousemove --sync --window "$WID" "$((T4_X + T4_W - 8))" "$((BOARD_TOP + A2_LANES_PADDING_TOP + A2_HEAD_H / 2))"
xdotool click 1
sleep 0.2
park_pointer 0.1
xdotool click 1
sleep 0.2
inject "$(board_message "$WORKSPACE" busy)" 0.08
shot a2-collapsed-busy.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-collapsed-busy.png "$BOARD_TOP" busy --width "$WIN_W"
inject "$(board_message "$WORKSPACE" busy)" 0.05
shot a2-pin-blocked-base.png
load_tracks /output/a2-pin-blocked-base.png
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
xdotool click 1
sleep 0.2
park_pointer 0.1
shot a2-pinned-blocked.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-pinned-blocked.png "$BOARD_TOP" pinned-blocked --width "$WIN_W"
load_tracks /output/a2-pinned-blocked.png
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W - 8))" "$((BOARD_TOP + A2_LANES_PADDING_TOP + A2_HEAD_H / 2))"
xdotool click 1
sleep 0.2
park_pointer 0.1
xdotool click 1
sleep 0.25

# Empty copy, whole-row overflow, 51px hover/focus, and three metadata columns.
show_board empty
shot a2-empty.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-empty.png "$BOARD_TOP" collapsed --width "$WIN_W"
show_board busy
shot a2-overflow.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-overflow.png "$BOARD_TOP" busy --width "$WIN_W"
python3 "$ORACLE" a2-metadata "$CONTRACT" /output/a2-overflow.png "$BOARD_TOP" 1 0
load_tracks /output/a2-overflow.png
ROW_Y=$((BOARD_TOP + A2_HEADBAND_H + A2_ROW_H / 2))
xdotool mousemove --sync --window "$WID" "$((T0_X + T0_W / 2))" "$ROW_Y"
sleep 0.2
shot a2-row-hover.png
python3 "$ORACLE" a2-row "$CONTRACT" /output/a2-overflow.png /output/a2-row-hover.png "$BOARD_TOP" 0 0 1.0
park_pointer 0.15
load_tracks /output/a2-overflow.png
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
xdotool click 1
sleep 0.12
shot a2-row-focus-base.png
xdotool key --clearmodifiers shift+Tab
sleep 0.25
shot a2-row-focus.png
python3 "$ORACLE" a2-row "$CONTRACT" /output/a2-row-focus-base.png /output/a2-row-focus.png "$BOARD_TOP" 2 2 1.0 --kind focus
load_tracks /output/a2-row-focus-base.png
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W - 8))" "$((BOARD_TOP + A2_LANES_PADDING_TOP + A2_HEAD_H / 2))"
xdotool click 1
sleep 0.2
park_pointer 0.1
xdotool click 1
sleep 0.2

# Native drag: source dims; 320x36 ghost follows; Done accepts, Blocked rejects.
open_badge
inject "$(board_message "$WORKSPACE" busy)" 0.05
shot a2-drag-ground.png
load_tracks /output/a2-drag-ground.png
SOURCE_X=$T1_X
SOURCE_Y=$((BOARD_TOP + A2_HEADBAND_H))
PRESS_X=$((SOURCE_X + T1_W - 24))
PRESS_Y=$((SOURCE_Y + 12))
xdotool mousemove --sync --window "$WID" "$PRESS_X" "$PRESS_Y"
sleep 0.2
shot a2-drag-base.png
xdotool mousedown 1
ARM_X=$((PRESS_X + 3))
xdotool mousemove --sync --window "$WID" "$ARM_X" "$PRESS_Y"
sleep 0.15
TARGET_X=$((T4_X + T4_W / 2))
TARGET_Y=$PRESS_Y
xdotool mousemove --sync --window "$WID" "$TARGET_X" "$TARGET_Y"
sleep 0.18
shot a2-drag-done-target.png
GHOST_X=$((TARGET_X - (ARM_X - SOURCE_X)))
GHOST_Y=$((TARGET_Y - (PRESS_Y - SOURCE_Y)))
python3 "$ORACLE" a2-drag "$CONTRACT" /output/a2-drag-base.png /output/a2-drag-done-target.png \
    "$GHOST_X" "$GHOST_Y" "$SOURCE_X" "$SOURCE_Y" "$T1_W" "$A2_ROW_H" \
    "$T4_X" "$BOARD_TOP" "$T4_W" "$A2_STRIP_H" \
    "$T3_X" "$BOARD_TOP" "$T3_W" "$A2_STRIP_H"
xdotool mouseup 1
sleep 0.3

# Text scale uses the contract's left-gutter steppers and keeps the reservation.
show_board busy
PLUS_X=$((A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W / 2))
MINUS_X=$((A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W + A2_ZOOM_GAP + A2_ZOOM_GLYPH_W / 2))
ZOOM_Y=$((BOARD_TOP + A2_ZOOM_TOP + A2_ZOOM_GLYPH_H / 2))
scale_steps() {
    local x=$1 count=$2
    for _ in $(seq 1 "$count"); do
        show_board busy
        xdotool mousemove --sync --window "$WID" "$x" "$ZOOM_Y"
        xdotool click 1
        sleep 0.06
    done
}
scale_steps "$MINUS_X" 10
scale_steps "$PLUS_X" 2
show_board busy
shot a2-scale-1.0.png
load_tracks /output/a2-scale-1.0.png
xdotool mousemove --sync --window "$WID" "$((T0_X + T0_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + A2_ROW_H / 2))"
sleep 0.18
shot a2-scale-1.0-hover.png
python3 "$ORACLE" a2-row "$CONTRACT" /output/a2-scale-1.0.png /output/a2-scale-1.0-hover.png "$BOARD_TOP" 0 0 1.0
scale_steps "$PLUS_X" 6
show_board busy
shot a2-scale-1.6.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-scale-1.6.png "$BOARD_TOP" busy --width "$WIN_W" --scale 1.6
load_tracks /output/a2-scale-1.6.png
xdotool mousemove --sync --window "$WID" "$((T0_X + T0_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + A2_ROW_H * 16 / 20 / 2))"
sleep 0.18
shot a2-scale-1.6-hover.png
python3 "$ORACLE" a2-row "$CONTRACT" /output/a2-scale-1.6.png /output/a2-scale-1.6-hover.png "$BOARD_TOP" 0 0 1.6
scale_steps "$MINUS_X" 10
show_board busy
shot a2-scale-0.8.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-scale-0.8.png "$BOARD_TOP" busy --width "$WIN_W" --scale 0.8
load_tracks /output/a2-scale-0.8.png
xdotool mousemove --sync --window "$WID" "$((T0_X + T0_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + A2_ROW_H * 8 / 20 / 2))"
sleep 0.18
shot a2-scale-0.8-hover.png
python3 "$ORACLE" a2-row "$CONTRACT" /output/a2-scale-0.8.png /output/a2-scale-0.8-hover.png "$BOARD_TOP" 0 0 0.8
scale_steps "$PLUS_X" 2

# Resize adds only whole 51px rows and leaves remainder as board ground.
show_board busy
shot a2-resize-before.png
RESIZE_DRAG=$((A2_ROW_H + 10))
FLOOR_Y=$((BOARD_TOP + A2_STRIP_H - 1))
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$FLOOR_Y"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((FLOOR_Y + RESIZE_DRAG))"
xdotool mouseup 1
sleep 0.05
shot a2-resized.png
show_board busy
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((FLOOR_Y + RESIZE_DRAG))"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$FLOOR_Y"
xdotool mouseup 1
sleep 0.2
python3 "$ORACLE" a2-resize "$CONTRACT" /output/a2-resize-before.png /output/a2-resized.png "$BOARD_TOP" "$RESIZE_DRAG"

# Opened, pointer trace, state dots/wires, live halo, and focus trace.
enter_flow flow standard
shot a3-opened.png
cp /output/a3-opened.png /output/a3-theme-before.png
python3 "$ORACLE" flow-opened "$CONTRACT" /output/a3-opened.png "$BOARD_TOP"
FLOW_CURSOR_X=$((A3_LEFT_PAD + 2 * A3_RANK_PITCH + 90))
FLOW_CURSOR_Y=$((BOARD_TOP + A3_GRAPH_TOP + (A3_GRAPH_H - (2 * A3_NODE_H + A3_ROW_GAP)) / 2 + A3_NODE_H / 2))
xdotool mousemove --sync --window "$WID" "$FLOW_CURSOR_X" "$FLOW_CURSOR_Y"
sleep 0.35
shot a3-traced-pointer.png
python3 "$ORACLE" flow-trace "$CONTRACT" /output/a3-opened.png /output/a3-traced-pointer.png "$BOARD_TOP"
board_park 0.35
SESSION=$(latest_session)
[ -n "$SESSION" ] || fail "no session id for live Flow fixture"
inject "{\"type\":\"IssueFocused\",\"session_id\":\"$SESSION\",\"issue_id\":\"fl-c\"}" 0.25
shot a3-live.png
python3 "$ORACLE" flow-live "$CONTRACT" /output/a3-opened.png /output/a3-live.png "$BOARD_TOP"
inject "{\"type\":\"IssueFocused\",\"session_id\":\"$SESSION\",\"issue_id\":null}" 0.2
xdotool mousemove --sync --window "$WID" "$FLOW_CURSOR_X" "$FLOW_CURSOR_Y"
xdotool click 1
sleep 0.2
board_park 0.35
shot a3-traced-focus.png
python3 "$ORACLE" flow-trace "$CONTRACT" /output/a3-opened.png /output/a3-traced-focus.png "$BOARD_TOP"

# Back control keeps visible focus and pointer activation returns to Lanes.
xdotool key --clearmodifiers Escape
sleep 0.25
enter_flow flow standard
shot a3-back-focus-base.png
xdotool mousemove --sync --window "$WID" "$((A3_LEFT_PAD + 80))" "$((BOARD_TOP + A3_GRAPH_TOP + A3_GRAPH_H / 2))"
xdotool click 1
xdotool key --clearmodifiers shift+Tab
xdotool key --clearmodifiers shift+Tab
sleep 0.3
shot a3-back-focus.png
python3 "$ORACLE" focus-control "$CONTRACT" /output/a3-back-focus-base.png /output/a3-back-focus.png "$BOARD_TOP" back
xdotool mousemove --sync --window "$WID" "$((A3_BAND_PAD_LEFT + 24))" "$((BOARD_TOP + A3_BAND_H / 2))"
xdotool click 1
sleep 0.5
board_park 0.2
shot a3-back-returned.png
[ "$(strip_diff /output/a3-back-focus.png /output/a3-back-returned.png)" -ge 500 ] || fail "pointer Back control did not return to Lanes"

# The mode-pair LANES control is the second Tab stop and activates by Enter.
enter_flow flow standard
shot a3-lanes-focus-base.png
xdotool mousemove --sync --window "$WID" "$((A3_LEFT_PAD + 80))" "$((BOARD_TOP + A3_GRAPH_TOP + A3_GRAPH_H / 2))"
xdotool click 1
xdotool key --clearmodifiers shift+Tab
sleep 0.3
shot a3-lanes-focus.png
python3 "$ORACLE" focus-control "$CONTRACT" /output/a3-lanes-focus-base.png /output/a3-lanes-focus.png "$BOARD_TOP" lanes
xdotool key --clearmodifiers Return
sleep 0.5
board_park 0.2
shot a3-lanes-returned.png
[ "$(strip_diff /output/a3-lanes-focus.png /output/a3-lanes-returned.png)" -ge 500 ] || fail "keyboard LANES control did not return to Lanes"

# Theme rewrite moves A2 and A3 semantic regions without moving geometry.
cp /output/a2-collapsed-busy.png /output/a2-theme-before.png
CONFIG_DIR="$XDG_CONFIG_HOME/scribe"
CONFIG_FILE="$CONFIG_DIR/config.toml"
mkdir -p "$CONFIG_DIR"
printf '[appearance]\ntheme = "dracula"\nanimations = false\n' >"$CONFIG_FILE"
wait_theme_reload
sleep 0.5
show_board busy
shot a2-theme-after.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-theme-after.png "$BOARD_TOP" busy --width "$WIN_W"
python3 "$ORACLE" theme "$CONTRACT" /output/a2-theme-before.png /output/a2-theme-after.png "$BOARD_TOP" a2
enter_flow flow standard
shot a3-theme-after.png
python3 "$ORACLE" flow-opened "$CONTRACT" /output/a3-theme-after.png "$BOARD_TOP"
python3 "$ORACLE" theme "$CONTRACT" /output/a3-theme-before.png /output/a3-theme-after.png "$BOARD_TOP" a3
xdotool mousemove --sync --window "$WID" "$((A3_BAND_PAD_LEFT + 24))" "$((BOARD_TOP + A3_BAND_H / 2))"
xdotool click 1
sleep 0.35
printf '[appearance]\ntheme = "minimal-dark"\nanimations = false\n' >"$CONFIG_FILE"
wait_theme_reload
sleep 0.5

# Four-row deep frontier, near/middle/far fades and position thumb.
enter_flow deep deep
inject "{\"type\":\"IssueFocused\",\"session_id\":\"$SESSION\",\"issue_id\":\"deep-z\"}" 0.2
board_park 0.2
shot a3-deep-near.png
python3 "$ORACLE" flow-overflow "$CONTRACT" /output/a3-deep-near.png "$BOARD_TOP" near
DEEP_FIRST_Y=$((BOARD_TOP + A3_GRAPH_TOP + (A3_GRAPH_H - (4 * A3_NODE_H + 3 * A3_ROW_GAP)) / 2 + A3_NODE_H / 2))
xdotool mousemove --sync --window "$WID" "$((A3_LEFT_PAD + 80))" "$DEEP_FIRST_Y"
xdotool click 1
xdotool key --clearmodifiers shift+Tab
sleep 0.25
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((BOARD_TOP + A3_GRAPH_TOP + 60))"
xdotool click 5
sleep 0.3
xdotool mousemove --sync --window "$WID" "$((WIN_W - A3_FADE_W - A3_NODE_W / 4))" "$((BOARD_TOP + A3_GRAPH_TOP + A3_GRAPH_H / 2))"
sleep 0.25
shot a3-deep-middle.png
python3 "$ORACLE" flow-overflow "$CONTRACT" /output/a3-deep-middle.png "$BOARD_TOP" middle
xdotool mousemove --sync --window "$WID" 300 "$((BOARD_TOP + A3_GRAPH_TOP + A3_GRAPH_H / 2))"
xdotool click 1
for _ in $(seq 1 5); do xdotool key --clearmodifiers shift+Tab; done
sleep 0.2
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((BOARD_TOP + A3_GRAPH_TOP + 60))"
for _ in $(seq 1 30); do xdotool click 5; done
sleep 0.4
shot a3-deep-far.png
python3 "$ORACLE" flow-overflow "$CONTRACT" /output/a3-deep-far.png "$BOARD_TOP" far
for _ in $(seq 1 40); do xdotool click 4; done
sleep 0.35

# Keyboard focus on the far node auto-scrolls it fully into view.
xdotool mousemove --sync --window "$WID" "$((A3_LEFT_PAD + 80))" "$DEEP_FIRST_Y"
xdotool click 1
for _ in $(seq 1 10); do xdotool key --clearmodifiers Tab; done
sleep 0.6
shot a3-keyboard-far-focus.png
python3 "$ORACLE" flow-overflow "$CONTRACT" /output/a3-keyboard-far-focus.png "$BOARD_TOP" far --skip-fades

# Narrow full-width state, then two independent split-region boards. The
# persisted Blocked pin auto-collapses only when the split would starve A2.
xdotool key --clearmodifiers Escape
sleep 0.4
ensure_pinned_board busy
shot a2-pre-narrow.png
load_tracks /output/a2-pre-narrow.png
xdotool mousemove --sync --window "$WID" "$((T3_X + T3_W / 2))" "$((BOARD_TOP + A2_HEADBAND_H + 30))"
sleep 0.25
xdotool click 1
sleep 0.35
xdotool windowsize --sync "$WID" 720 739
sleep 0.6
focus
inject "$(board_message "$WORKSPACE" busy)"
park_pointer 0.2
shot a2-narrow.png
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-narrow.png "$BOARD_TOP" pinned-blocked --width "$WIN_W"
xdotool windowsize --sync "$WID" 1008 739
sleep 0.5
focus
xdotool key --clearmodifiers ctrl+alt+backslash
for _ in $(seq 1 40); do
    SECOND=$(other_workspace "$WORKSPACE" 2>/dev/null || true)
    [ -n "${SECOND:-}" ] && [ "$(latest_rows "$SECOND")" -gt 0 ] && break
    sleep 0.2
done
[ -n "${SECOND:-}" ] || fail "split did not create a second workspace"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":null}"
inject "$(board_message "$WORKSPACE" busy)"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$SECOND\",\"name\":\"second\",\"accent_color\":\"#22d3ee\",\"split_direction\":null,\"project_root\":null}"
inject "$(board_message "$SECOND" busy)"
if [ "$(latest_rows "$WORKSPACE")" -gt "$PINNED_ROWS" ]; then
    open_badge
    xdotool click 1
    sleep 0.25
    inject "$(board_message "$WORKSPACE" busy)" 0.05
fi
if [ "$(latest_rows "$SECOND")" -gt "$PINNED_ROWS" ]; then
    open_badge "$((WIN_W / 2))"
    xdotool click 1
    sleep 0.25
    inject "$(board_message "$SECOND" busy)" 0.05
fi
park_pointer 0.2
shot a2-narrow-split.png
HALF=$((WIN_W / 2))
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-narrow-split.png "$BOARD_TOP" auto-collapsed --left 0 --width "$HALF"
python3 "$ORACLE" a2-layout "$CONTRACT" /output/a2-narrow-split.png "$BOARD_TOP" busy --left "$HALF" --width "$HALF"

python3 "$ORACLE" inventory "$CONTRACT" /output /output/beads-a2a3-contract-evidence.json

echo "PASS: generated A2/A3 contract $CONTRACT_SOURCE_SHA; states=$CONTRACT_STATE_SLUGS;" \
    "A2 sparse/busy/drawers/pins/empty/overflow/drag/scales/resize/theme/narrow-split;" \
    "A3 opened/trace/live/controls/deep/near-middle-far/fades/hbar/focus/theme;" \
    "reduced-motion=true rows=$UNPINNED_ROWS->$PINNED_ROWS"
