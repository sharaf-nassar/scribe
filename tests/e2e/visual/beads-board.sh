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
LONG_TITLE="Normal-card hover reveals this complete long Beads title in an opaque theme-derived bounded wrapping viewport-safe tooltip"

fail() {
    echo "FAIL: $1"
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
    WIN_X=$X WIN_Y=$Y WIN_W=$WIDTH WIN_H=$HEIGHT
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.8
}

first_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    found = None
    for line in fh:
        try: row = json.loads(line)
        except ValueError: continue
        msg = row.get("message", {})
        if row.get("dir") == "client" and msg.get("type") == "CreateSession":
            found = msg["workspace_id"]
    if found:
        print(found)
        raise SystemExit
raise SystemExit(1)
PY
}

sample_board() {
    local workspace="$1" now
    now=$(date +%s%3N)
    # The four-issue lanes the mock itself shows, so a capture here is
    # comparable to .impeccable/mocks/beads-compact-live-overview.html.
    printf '%s' "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$workspace\",\"protocol_version\":1,\"state\":{\"Ready\":{\"snapshot\":{\"refreshed_at_epoch_ms\":$now,\"backlog\":[{\"id\":\"sc-70\",\"title\":\"Document cache policy\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-81\",\"title\":\"Parse custom statuses\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-76\",\"title\":\"Expose stale timestamp\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-64\",\"title\":\"Polish empty queue copy\",\"priority\":4,\"blocker_ids\":[],\"parent_epic_name\":null}],\"ready\":[{\"id\":\"sc-88\",\"title\":\"$LONG_TITLE\",\"priority\":0,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-94\",\"title\":\"Cache stale state\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-32\",\"title\":\"Wire workspace refresh\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Workspace intelligence and rooted boards\"},{\"id\":\"sc-27\",\"title\":\"Add unavailable state\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null}],\"in_progress\":[{\"id\":\"sc-1bf\",\"title\":\"$LONG_TITLE\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Workspace intelligence and rooted boards\"},{\"id\":\"sc-43\",\"title\":\"Render queue rail\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-55\",\"title\":\"Pin board across tabs\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":\"GPUI client rebuild\"},{\"id\":\"sc-49\",\"title\":\"Preserve lane scroll\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null}],\"blocked\":[{\"id\":\"sc-91\",\"title\":\"Sync conflict handling\",\"priority\":1,\"blocker_ids\":[\"sc-12\",\"sc-19\"],\"parent_epic_name\":null},{\"id\":\"sc-58\",\"title\":\"Remote workspace reads\",\"priority\":2,\"blocker_ids\":[\"sc-22\"],\"parent_epic_name\":null},{\"id\":\"sc-36\",\"title\":\"Restore cached snapshot\",\"priority\":3,\"blocker_ids\":[\"sc-08\"],\"parent_epic_name\":null},{\"id\":\"sc-18\",\"title\":\"Resolve Dolt lock state\",\"priority\":2,\"blocker_ids\":[\"sc-07\"],\"parent_epic_name\":null}],\"done\":[{\"id\":\"sc-61\",\"title\":\"$LONG_TITLE\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":\"GPUI client rebuild\"},{\"id\":\"sc-24\",\"title\":\"Detect workspace root\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-16\",\"title\":\"Cache bd availability\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-09\",\"title\":\"Tune titlebar spacing\",\"priority\":4,\"blocker_ids\":[],\"parent_epic_name\":null}],\"backlog_total\":12,\"ready_total\":4,\"in_progress_total\":5,\"blocked_total\":4,\"done_total\":38},\"stale\":false,\"refresh_error\":null}}}"
}

# The same lanes, with the Ready lane's second card carrying the epic id that
# makes it Flow-eligible. `parent_epic_id` is `#[serde(default)]` and decides
# eligibility client-side; `parent_epic_name` stays null so the card paints
# exactly as it does in `sample_board` and no lane geometry moves.
flow_board() {
    local workspace="$1"
    sample_board "$workspace" | python3 -c '
import json, sys
board = json.load(sys.stdin)
for item in board["state"]["Ready"]["snapshot"]["ready"]:
    if item["id"] == "sc-94":
        item["parent_epic_id"] = "fl-epic"
json.dump(board, sys.stdout)
'
}

# A five-rank epic whose interior ranks each carry two nodes, so every probe
# below can sit on rank 1 or 2 and still distinguish "styled" from "the only
# one" — the degenerate-fixture trap recorded in
# docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md.
#
# sc-94 is the cursor because it is the card the click above opens. Ranks run
# a -> {b,c} -> {sc-94,e} -> f -> g, and the a->f edge deliberately skips two
# ranks so the router has to emit dummies. fl-a's two out-edges share one
# vertical gutter, which is what makes the interval-union half-lighting
# observable: tracing sc-94 lights the fl-b half and dims the fl-c half of the
# same run.
flow_epic_graph() {
    local workspace="$1"
    python3 -c '
import json, sys
node = lambda i, t, p, s, q, a=None: {
    "id": i, "title": t, "priority": p, "status": s, "queue": q,
    "assignee": a, "updated_at": "2026-08-18T00:00:00Z",
}
graph = {
    "epic_id": "fl-epic", "epic_title": "Flow epic", "closed": 2, "total": 7,
    "nodes": [
        node("fl-a", "Alpha root", 1, "closed", "done"),
        node("fl-b", "Bravo blocked", 1, "open", "blocked"),
        node("fl-c", "Charlie ready", 2, "open", "ready", "codex-agent-run"),
        node("sc-94", "Cache stale state", 1, "open", "in_progress"),
        node("fl-e", "Echo done", 3, "closed", "done"),
        node("fl-f", "Foxtrot ready", 2, "open", "ready"),
        node("fl-g", "Golf backlog", 4, "open", "backlog"),
    ],
    "edges": [
        {"from": "fl-a", "to": "fl-b"}, {"from": "fl-a", "to": "fl-c"},
        {"from": "fl-b", "to": "sc-94"}, {"from": "fl-c", "to": "fl-e"},
        {"from": "sc-94", "to": "fl-f"}, {"from": "fl-e", "to": "fl-f"},
        {"from": "fl-f", "to": "fl-g"}, {"from": "fl-a", "to": "fl-f"},
    ],
}
json.dump({"type": "BeadsEpicGraph", "workspace_id": sys.argv[1],
           "epic_id": "fl-epic", "outcome": {"graph": graph}}, sys.stdout)
' "$workspace"
}

# A board whose queues have all run dry, which is the state the lane heads would
# otherwise float above nothing in.
empty_board() {
    local workspace="$1" now
    now=$(date +%s%3N)
    printf '%s' "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$workspace\",\"protocol_version\":1,\"state\":{\"Ready\":{\"snapshot\":{\"refreshed_at_epoch_ms\":$now,\"backlog\":[],\"ready\":[],\"in_progress\":[],\"blocked\":[],\"done\":[],\"backlog_total\":0,\"ready_total\":0,\"in_progress_total\":0,\"blocked_total\":0,\"done_total\":0},\"stale\":false,\"refresh_error\":null}}}"
}

# Pixels between the right edge of the epic tag on a card's id-and-epic line and
# the right edge of the card's content box. The epic is right-aligned, so this
# is the tag's own corner rounding and nothing more.
epic_right_gap() {
    local dump=/tmp/epic-band.txt
    convert "$1" -crop "${2}x${3}+0+${4}" +repage txt:- >"$dump"
    python3 - "$dump" "$2" <<'PY'
import re, sys
width = int(sys.argv[2])
lanes_pad, lanes = 8.0, 5
lane_w = (width - 2 * lanes_pad) / lanes
pixels = {}
for line in open(sys.argv[1]):
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if m:
        x, y, r, g, b = (int(v) for v in m.groups())
        pixels[(x, y)] = (r, g, b)
rows = sorted({y for _, y in pixels})
worst, measured = 0, 0
for lane in range(lanes):
    left = lanes_pad + lane_w * lane
    # The card is inset by the lane's padding and the lane body's scroll
    # gutter; inside that sit its border and its own right padding.
    card_right = int(lanes_pad + lane_w * (lane + 1) - 8 - 4)
    content = card_right - 1 - 8
    ink = 0
    for y in rows:
        # The card's own fill, read from between the tag and the border, so a
        # tag that ends early is the only thing this can find.
        base = pixels.get((card_right - 3, y))
        if base is None:
            continue
        for x in range(card_right - 3, int(left), -1):
            if sum(abs(a - b) for a, b in zip(pixels[(x, y)], base)) > 30:
                ink = max(ink, x)
                break
    # Only a card that has an epic says anything about where an epic sits; a
    # card with just its id leaves the right half of the line as bare fill.
    if ink < left + lane_w * 0.5:
        continue
    measured += 1
    worst = max(worst, content - ink)
print(worst if measured else 999)
PY
}

# How much colour a priority's badge carries over the card it sits on, measured
# against bare card fill and read from the badge's own padding.
edge_delta() {
    convert "$1" -format \
        "%[fx:round(255*(abs(p{$2,$3}.r-p{350,112}.r)+abs(p{$2,$3}.g-p{350,112}.g)+abs(p{$2,$3}.b-p{350,112}.b)))]" \
        info:
}

# The y of the board's bottom bar, found from the last long run of the
# captured board ground. A split can put unrelated chrome above the board, so
# the first colour transition in a fixed-height crop is not the board edge.
board_bottom() {
    local dump=/tmp/board-column.txt
    convert "$1" -crop "1x$((WIN_H - 40))+4+40" +repage txt:- >"$dump"
    python3 - "$dump" "$BOARD_GROUND" <<'PY'
import re, sys
rows = []
for line in open(sys.argv[1]):
    m = re.match(r'\d+,(\d+): \((\d+),(\d+),(\d+)', line)
    if m:
        rows.append((int(m.group(1)) + 40, tuple(int(m.group(i)) for i in (2, 3, 4))))
ground = tuple(map(int, re.findall(r'\d+', sys.argv[2])[:3]))
start = end = bottom = None
for y, color in sorted(rows):
    if sum(abs(a - b) for a, b in zip(color, ground)) <= 12:
        if start is None:
            start = y
        end = y
    elif start is not None:
        if end - start + 1 >= 30:
            bottom = end
        start = end = None
if start is not None and end - start + 1 >= 30:
    bottom = end
print(bottom or 999)
PY
}

# One pixel, as `srgb(r,g,b)`. Every Flow probe reads through this so a sample
# site is a coordinate and never a hardcoded expectation.
px_at() {
    convert "$1" -format "%[pixel:p{$2,$3}]" info:
}

# Manhattan distance between two `srgb(...)` strings. Flow sites are
# antialiased — several dot centres land on a half pixel — so every colour
# assertion is a tolerance, never equality.
px_delta() {
    python3 -c '
import re, sys
a = [int(v) for v in re.findall(r"\d+", sys.argv[1])[:3]]
b = [int(v) for v in re.findall(r"\d+", sys.argv[2])[:3]]
print(sum(abs(x - y) for x, y in zip(a, b)))
' "$1" "$2"
}

# The longest vertical run of non-ground pixels in the graph's rightmost
# columns. A horizontal bar is the only scrollbar Flow has, so this must stay
# far below the graph's own height in every state. The ground is sampled from
# the same image rather than pinned, because a theme edit moves it — pinning it
# reported a full-height "bar" on a dracula capture that was only the new
# ground.
right_edge_run() {
    local image="$1" ground
    ground=$(px_at "$image" 5 "$((FLOW_GRAPH_TOP + 20))")
    convert "$image" -crop "6x${FLOW_GRAPH_H}+$((WIN_W - 6))+${FLOW_GRAPH_TOP}" +repage txt:- |
        python3 -c '
import re, sys
ground = [int(v) for v in re.findall(r"\d+", sys.argv[1])[:3]]
cols = {}
for line in sys.stdin:
    m = re.match(r"(\d+),(\d+): \((\d+),(\d+),(\d+)", line)
    if m:
        x, y = int(m.group(1)), int(m.group(2))
        cols.setdefault(x, []).append((y, [int(m.group(i)) for i in (3, 4, 5)]))
worst = 0
for run in cols.values():
    current = 0
    for _, colour in sorted(run):
        if sum(abs(a - b) for a, b in zip(colour, ground)) > 10:
            current += 1
            worst = max(worst, current)
        else:
            current = 0
print(worst)
' "$ground"
}

# Pixels that changed inside the pinned strip between two captures. Scoped to
# the strip on purpose: the detail panel under the board repaints on its own,
# so a whole-window diff cannot answer "did the strip move".
strip_diff() {
    convert "$1" "$2" -compose difference -composite -colorspace Gray \
        -crop "${WIN_W}x$((FLOW_FLOOR_BOTTOM - BOARD_TOP))+0+${BOARD_TOP}" +repage \
        -threshold 10% -format "%[fx:round(mean*w*h)]" info:
}

board_request_count() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
count = 0
with open(sys.argv[1]) as fh:
    for line in fh:
        try: row = json.loads(line)
        except ValueError: continue
        msg = row.get("message", {})
        if (row.get("dir") == "client" and msg.get("type") == "RequestBeadsBoard"
                and msg.get("workspace_id") == sys.argv[2]):
            count += 1
print(count)
PY
}

latest_rows() {
    python3 - "$CLIENT_LOG" "${1:-}" <<'PY'
import re, sys
ansi = re.compile(r'\x1b\[[0-9;]*m')
want = sys.argv[2] if len(sys.argv) > 2 else ""
if want and not want.startswith("ws-"):
    want = f"ws-{want[:8]}"
rows = []
with open(sys.argv[1], errors='replace') as fh:
    for line in fh:
        line = ansi.sub('', line)
        if "published a pane's grid size" not in line:
            continue
        if want and f"workspace_id={want}" not in line:
            continue
        match = re.search(r'rows=(\d+)', line)
        if match: rows.append(int(match.group(1)))
print(rows[-1] if rows else 0)
PY
}

# Crop the 16px connected-node mark from a badge target. The target is 26px
# wide, so its centered mark sits 13px from the badge's leading edge. Compare
# the foreground masks because each bar blends antialiasing into its own fill.
badge_mark() {
    local image="$1" center_x="$2" center_y="$3" output="$4" background
    background=$(convert "$image" \
        -format "%[pixel:p{$((center_x - 8)),$((center_y - 8))}]" info:)
    convert "$image" -crop "16x16+$((center_x - 8))+$((center_y - 8))" \
        +repage -transparent "$background" -alpha extract "$output"
}

assert_matching_badge_marks() {
    local title_mark="$1" region_mark="$2" ink diff
    ink=$(identify -format '%[fx:mean*w*h]' "$title_mark")
    ink=${ink%%.*}
    [ "${ink:-0}" -eq 59 ] ||
        fail "titlebar Beads mark has ${ink:-0}px of foreground, expected 59px"
    diff=$(compare -metric AE "$title_mark" "$region_mark" null: 2>&1 || true)
    diff=${diff%%.*}
    [ "${diff:-1}" -eq 0 ] ||
        fail "titlebar and lower-region Beads marks differ by ${diff:-unknown}px"
}

# Measure one synchronized native-drag waypoint against pointer minus GPUI's
# threshold-crossing offset. ImageMagick reports the trimmed delta's `%X/%Y`.
assert_drag_frame() {
    local current="$1" pointer_x="$2" pointer_y="$3" label="$4" target_border_x="${5:-}"
    local expected_x expected_y crop_x crop_y border_crop_x tip_center_x mask_draw bounds
    local ghost_x ghost_y ghost_w ghost_h rows
    expected_x=$((pointer_x - ARM_OFFSET_X))
    expected_y=$((pointer_y - PRESS_OFFSET_Y))
    crop_x=$((expected_x - 8))
    crop_y=$((expected_y - 8))
    # The base frame is captured mid-hover on the source card, before the drag
    # arms and GPUI suppresses that tooltip, so a long-titled source's centred
    # reveal can still be on screen there and gone by every later frame. Mask
    # its full possible footprint -- centred on the source card, bounded by
    # the tooltip's own proven 500px width and 100px height ceilings above it
    # -- out of every waypoint rather than just the one its span happens to
    # reach.
    tip_center_x=$((SOURCE_LEFT + CARD_W / 2))
    mask_draw="rectangle $((tip_center_x - 250 - crop_x)),$((SOURCE_TOP - 104 - crop_y)) $((tip_center_x + 250 - crop_x)),$((SOURCE_TOP - crop_y))"
    # Target borders are asserted independently at their waypoints. Remove
    # only that known two-pixel raster footprint from the ghost's delta.
    if [ -n "$target_border_x" ]; then
        border_crop_x=$((target_border_x - crop_x))
        mask_draw="$mask_draw rectangle ${border_crop_x},0 $((border_crop_x + 1)),$((CARD_H + 15))"
    fi
    bounds=$(convert /output/beads-board-drag-base.png "$current" \
        -compose difference -composite \
        -crop "$((CARD_W + 16))x$((CARD_H + 16))+${crop_x}+${crop_y}" +repage \
        -colorspace Gray -threshold 12% -fill black -draw "$mask_draw" \
        -trim -format '%X,%Y,%w,%h' info:)
    if [[ "$bounds" =~ ^\+([0-9]+),\+([0-9]+),([0-9]+),([0-9]+)$ ]]; then
        ghost_x=$((crop_x + BASH_REMATCH[1]))
        ghost_y=$((crop_y + BASH_REMATCH[2]))
        ghost_w=${BASH_REMATCH[3]}
        ghost_h=${BASH_REMATCH[4]}
    else
        fail "could not measure $label drag ghost (${bounds:-empty})"
    fi
    [ "$((ghost_x - expected_x))" -ge -3 ] &&
        [ "$((ghost_x - expected_x))" -le 3 ] &&
        [ "$((ghost_y - expected_y))" -ge -3 ] &&
        [ "$((ghost_y - expected_y))" -le 3 ] &&
        [ "$ghost_w" -ge "$((CARD_W - 6))" ] &&
        [ "$ghost_h" -ge "$((CARD_H - 6))" ] ||
        fail "$label ghost ${ghost_w}x${ghost_h}+${ghost_x}+${ghost_y}, expected ${CARD_W}x${CARD_H}+${expected_x}+${expected_y} within 3px"
    rows=$(latest_rows)
    [ "$rows" -eq "$BASE_ROWS" ] ||
        fail "$label drag changed terminal rows ($BASE_ROWS -> $rows)"
}

# The newest workspace the server described that is not $1.
other_workspace() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
found = None
with open(sys.argv[1]) as fh:
    for line in fh:
        try: row = json.loads(line)
        except ValueError: continue
        msg = row.get("message", {})
        if (row.get("dir") == "server" and msg.get("type") == "WorkspaceInfo"
                and msg.get("workspace_id") != sys.argv[2]):
            found = msg["workspace_id"]
if not found:
    raise SystemExit(1)
print(found)
PY
}

# Every workspace that has published a pane size, most recent last.
published_workspaces() {
    python3 - "$CLIENT_LOG" <<'PY'
import re, sys
ansi = re.compile(r'\x1b\[[0-9;]*m')
seen = []
with open(sys.argv[1], errors='replace') as fh:
    for line in fh:
        line = ansi.sub('', line)
        if "published a pane's grid size" not in line:
            continue
        match = re.search(r'workspace_id=(\S+)', line)
        if match and match.group(1) not in seen:
            seen.append(match.group(1))
print(" ".join(seen))
PY
}

# Every full workspace id the server has described, in first-seen order.
server_workspaces() {
    python3 - "$RECORD" <<'PY'
import json, sys
seen = []
with open(sys.argv[1]) as fh:
    for line in fh:
        try: row = json.loads(line)
        except ValueError: continue
        msg = row.get("message", {})
        if row.get("dir") != "server" or msg.get("type") != "WorkspaceInfo":
            continue
        workspace = msg["workspace_id"]
        if workspace not in seen:
            seen.append(workspace)
print(" ".join(seen))
PY
}

# Whether the real upstream server (never `inject`, which hands the client a
# message directly and is never recorded) has answered NotDetected for $1's
# Beads board on the wire, at or after record line $2. share-tap appends a
# genuine relayed frame to $RECORD strictly before handing it to the client's
# inbound channel, and an injection reaches that same channel only through a
# later call, so observing this is a real happens-before guarantee against a
# later `inject` reordering behind an in-flight real reply. The workspace can
# already carry earlier real NotDetected replies from long before the event
# under test, so a caller must pass the record's line count from just before
# the action that is expected to provoke a fresh one.
server_reported_not_detected() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
skip = int(sys.argv[3])
with open(sys.argv[1]) as fh:
    for i, line in enumerate(fh):
        if i < skip:
            continue
        try: row = json.loads(line)
        except ValueError: continue
        msg = row.get("message", {})
        if (row.get("dir") == "server" and msg.get("type") == "BeadsBoard"
                and msg.get("workspace_id") == sys.argv[2]
                and msg.get("state") == "NotDetected"):
            print("1")
            raise SystemExit
print("0")
PY
}

# Current line count of $RECORD, used to scope a later `server_reported_*`
# check to events at or after this point. wc -l undercounts a file with no
# trailing newline by one; share-tap's writer always ends a line with \n, so
# this is exact for a record that is not actively mid-write, and an
# undercount would only widen the scanned window, never narrow it.
record_mark() {
    wc -l <"$RECORD" 2>/dev/null || echo 0
}

sleep 2
focus
WID=$(window_id)
WORKSPACE=$(first_workspace) || fail "no SessionList workspace recorded"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":null}"
inject "$(sample_board "$WORKSPACE")"
import -window "$(window_id)" /output/beads-board-base.png
BASE_ROWS=$(latest_rows)
[ "$BASE_ROWS" -gt 0 ] || fail "no baseline grid geometry"
# Single-workspace titlebar: the graph target leads the label at the bar edge.
TOP_BADGE_ICON_X=13
TOP_BADGE_ICON_Y=17
xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
sleep 0.5
import -window "$WID" /output/beads-board-hover.png
HOVER_DIFF=$(compare -metric AE /output/beads-board-base.png /output/beads-board-hover.png null: 2>&1 || true)
HOVER_DIFF=${HOVER_DIFF%%.*}
[ "${HOVER_DIFF:-0}" -ge 10000 ] || fail "hover board changed only $HOVER_DIFF px"

# The board wears the theme: its ground is the same chrome slot the tab bar
# paints with, so the two must sample identically. Sampled in the strip's own
# left padding, which is the ground itself — inside a lane the cards and the
# queue wash are painted over it.
BOARD_GROUND=$(convert /output/beads-board-hover.png \
    -format "%[pixel:p{4,68}]" info:)
CHROME_GROUND=$(convert /output/beads-board-hover.png \
    -format "%[pixel:p{$((WIN_W - 200)),15}]" info:)
[ "$BOARD_GROUND" = "$CHROME_GROUND" ] ||
    fail "board ground $BOARD_GROUND is not the chrome's $CHROME_GROUND"

# The board stays neutral between the compact foreground marks. Every lane's
# bare lower area must match the board ground instead of carrying its queue
# colour as a full-height wash.
LANE_W=$(((WIN_W - 16) / 5))
for lane in 0 1 2 3 4; do
    LANE_GROUND=$(convert /output/beads-board-hover.png \
        -format "%[pixel:p{$((8 + lane * LANE_W + 4)),220}]" info:)
    [ "$LANE_GROUND" = "$BOARD_GROUND" ] ||
        fail "lane $lane ground $LANE_GROUND is tinted instead of neutral $BOARD_GROUND"
done

# An issue is a raised card, not a row on the bare strip: the fill inside it
# has to read lighter than the ground it sits on, in every theme, or the board
# is flat again. Sampled across the first card's lower padding, which carries
# no ink of its own.
CARD_FILL=$(convert /output/beads-board-hover.png \
    -crop 60x1+280+112 +repage -format "%[fx:mean]" info:)
GROUND_FILL=$(convert /output/beads-board-hover.png \
    -crop 1x1+4+112 +repage -format "%[fx:mean]" info:)
awk -v card="$CARD_FILL" -v ground="$GROUND_FILL" \
    'BEGIN { exit !(card > ground + 0.02) }' ||
    fail "the card fill ($CARD_FILL) does not sit above the ground ($GROUND_FILL)"

# A hotter priority wears a stronger badge. Read down the Ready lane's first
# three cards, which run P0, P1, P2 — the ranking has to hold across their
# three different hues, which is what the solved tint is for.
P0_EDGE=$(edge_delta /output/beads-board-hover.png 225 84)
P1_EDGE=$(edge_delta /output/beads-board-hover.png 225 134)
P2_EDGE=$(edge_delta /output/beads-board-hover.png 225 184)
# Compared as a ratio, not a margin: the ranks are a fifth apart by design, so
# a fixed number of levels would either pass a flat ramp or fail a fine one.
[ "$((${P0_EDGE:-0} * 100))" -gt "$((P1_EDGE * 115))" ] ||
    fail "the P0 badge (${P0_EDGE}) does not outrank P1's (${P1_EDGE})"
[ "$((${P1_EDGE:-0} * 100))" -gt "$((P2_EDGE * 115))" ] ||
    fail "the P1 badge (${P1_EDGE}) does not outrank P2's (${P2_EDGE})"

# The epic sits on the card's right edge. Measured on the id-and-epic line of
# the first card in each lane, three of which carry one.
EPIC_GAP=$(epic_right_gap /output/beads-board-hover.png "$WIN_W" 10 97)
[ "${EPIC_GAP:-999}" -le 6 ] || fail "the epic is ${EPIC_GAP}px short of the card's right edge"

# Hovering even the bottom padding of a truncated normal card reveals its
# complete title immediately above that card. The long Done card sits against
# the right viewport edge, so the wrapped popup also proves the bounded reveal
# stays inside the window. Mask the card's own hover repaint before measuring.
TITLE_CARD_LEFT=$((16 + 4 * LANE_W))
TITLE_CARD_TOP=70
TITLE_CARD_W=$((LANE_W - 20))
TITLE_CARD_H=46
xdotool mousemove --sync --window "$WID" \
    "$((TITLE_CARD_LEFT + TITLE_CARD_W - 10))" \
    "$((TITLE_CARD_TOP + TITLE_CARD_H - 5))"
sleep 0.1
import -window "$WID" /output/beads-board-title-tooltip.png
TOOLTIP_BOUNDS=$(convert /output/beads-board-hover.png \
    /output/beads-board-title-tooltip.png -compose difference -composite \
    -colorspace Gray -threshold 8% -fill black \
    -draw "rectangle 0,0 40,34 rectangle $((TITLE_CARD_LEFT - 4)),${TITLE_CARD_TOP} $((TITLE_CARD_LEFT + TITLE_CARD_W + 12)),$((TITLE_CARD_TOP + TITLE_CARD_H)) rectangle 0,300 ${WIN_W},${WIN_H}" \
    -trim -format '%X,%Y,%w,%h' info: 2>/dev/null || true)
if [[ "$TOOLTIP_BOUNDS" =~ ^\+([0-9]+),\+([0-9]+),([0-9]+),([0-9]+)$ ]]; then
    TOOLTIP_X=${BASH_REMATCH[1]}
    TOOLTIP_Y=${BASH_REMATCH[2]}
    TOOLTIP_W=${BASH_REMATCH[3]}
    TOOLTIP_H=${BASH_REMATCH[4]}
else
    fail "normal-card hover did not reveal the full-title tooltip (${TOOLTIP_BOUNDS:-empty})"
fi
[ "$TOOLTIP_W" -ge 300 ] &&
    [ "$TOOLTIP_W" -le 500 ] &&
    [ "$TOOLTIP_H" -ge 30 ] &&
    [ "$TOOLTIP_H" -le 100 ] &&
    [ "$TOOLTIP_X" -ge 0 ] &&
    [ "$((TOOLTIP_X + TOOLTIP_W))" -le "$WIN_W" ] &&
    [ "$TOOLTIP_X" -lt "$((TITLE_CARD_LEFT + TITLE_CARD_W))" ] &&
    [ "$((TOOLTIP_X + TOOLTIP_W))" -gt "$TITLE_CARD_LEFT" ] &&
    [ "$((TOOLTIP_Y + TOOLTIP_H))" -le "$TITLE_CARD_TOP" ] ||
    fail "full-title tooltip ${TOOLTIP_W}x${TOOLTIP_H}+${TOOLTIP_X}+${TOOLTIP_Y} is not above its card, wrapped, bounded, and viewport-safe"
TOOLTIP_BG=$(convert /output/beads-board-title-tooltip.png \
    -format "%[pixel:p{$((TOOLTIP_X + 3)),$((TOOLTIP_Y + 3))}]" info:)
[ "$TOOLTIP_BG" = "$BOARD_GROUND" ] ||
    fail "full-title tooltip background $TOOLTIP_BG is not the opaque theme ground $BOARD_GROUND"

# The tooltip has to centre on its own card, not track the card's left edge.
# Proven on the In Progress lane's first card, away from both viewport edges,
# so nothing here can be explained by the snap-to-window clamp above: that
# edge-clamp probe stays lane-4/left-anchored and unchanged.
CENTER_CARD_LEFT=$(( 16 + 2 * LANE_W ))
CENTER_CARD_TOP=70
CENTER_CARD_W=$(( LANE_W - 20 ))
CENTER_CARD_H=46
CENTER_CARD_CENTER_X=$(( CENTER_CARD_LEFT + CENTER_CARD_W / 2 ))
xdotool mousemove --sync --window "$WID" \
    "$(( CENTER_CARD_LEFT + CENTER_CARD_W - 10 ))" \
    "$(( CENTER_CARD_TOP + CENTER_CARD_H - 5 ))"
sleep 0.1
import -window "$WID" /output/beads-board-title-tooltip-center.png
CENTER_TOOLTIP_BOUNDS=$(convert /output/beads-board-hover.png \
    /output/beads-board-title-tooltip-center.png -compose difference -composite \
    -colorspace Gray -threshold 8% -fill black \
    -draw "rectangle 0,0 40,34 rectangle $(( CENTER_CARD_LEFT - 4 )),${CENTER_CARD_TOP} $(( CENTER_CARD_LEFT + CENTER_CARD_W + 12 )),$(( CENTER_CARD_TOP + CENTER_CARD_H )) rectangle 0,300 ${WIN_W},${WIN_H}" \
    -trim -format '%X,%Y,%w,%h' info: 2>/dev/null || true)
if [[ "$CENTER_TOOLTIP_BOUNDS" =~ ^\+([0-9]+),\+([0-9]+),([0-9]+),([0-9]+)$ ]]; then
    CENTER_TOOLTIP_X=${BASH_REMATCH[1]}
    CENTER_TOOLTIP_W=${BASH_REMATCH[3]}
else
    fail "middle-lane hover did not reveal the full-title tooltip (${CENTER_TOOLTIP_BOUNDS:-empty})"
fi
CENTER_TOOLTIP_CENTER_X=$(( CENTER_TOOLTIP_X + CENTER_TOOLTIP_W / 2 ))
CENTER_DELTA=$(( CENTER_TOOLTIP_CENTER_X - CENTER_CARD_CENTER_X ))
[ "$CENTER_DELTA" -ge -4 ] && [ "$CENTER_DELTA" -le 4 ] \
    || fail "middle-lane tooltip centred at ${CENTER_TOOLTIP_CENTER_X}px, card centre is ${CENTER_CARD_CENTER_X}px (${CENTER_DELTA}px off)"

# A card's id and epic are copy targets, which take the hover away from the
# board. The board has to survive the pointer landing on one, past the grace
# period a leave starts.
xdotool mousemove --sync --window "$WID" 29 99
sleep 0.6
import -window "$WID" /output/beads-board-card-hover.png
CARD_DIFF=$(compare -metric AE /output/beads-board-hover.png \
    /output/beads-board-card-hover.png null: 2>&1 || true)
CARD_DIFF=${CARD_DIFF%%.*}
# A card lights up under the pointer, so this is not a no-change check: the
# whole card repaints. A board that closed would take the entire strip with it,
# which is an order of magnitude more than the one card this allows.
[ "${CARD_DIFF:-999999}" -le 20000 ] ||
    fail "the board closed under the pointer on a card (${CARD_DIFF}px changed)"

# Clicking an id copies it in full, project prefix and all: the card shows the
# short form to save room, but a shortened id is not one bd would accept.
printf '%s' "clipboard-not-touched" | xclip -selection clipboard >/dev/null 2>&1
sleep 0.3
xdotool click 1
for _ in $(seq 1 20); do
    COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
    [ "$COPIED" = "sc-70" ] && break
    sleep 0.2
done
[ "${COPIED:-}" = "sc-70" ] || fail "clicking the id copied '${COPIED:-}' instead of sc-70"

# GPUI's native drag root must carry the long-titled source card above other
# cards and beyond the board's clipping edge without carrying a tooltip. Start
# in the Ready card's title row: its metadata line is a nested copy target that
# deliberately owns its own press.
CARD_W=$((LANE_W - 20))
CARD_H=46
SOURCE_LEFT=$((16 + LANE_W))
SOURCE_TOP=70
PRESS_X=$((SOURCE_LEFT + 70))
PRESS_Y=$((SOURCE_TOP + 14))
PRESS_OFFSET_Y=$((PRESS_Y - SOURCE_TOP))
TITLE_TOP=$((SOURCE_TOP + 6))
TITLE_BOTTOM=$((TITLE_TOP + 17))
META_TOP=$((TITLE_BOTTOM + 2))
[ "$PRESS_X" -gt "$SOURCE_LEFT" ] &&
    [ "$PRESS_X" -lt "$((SOURCE_LEFT + CARD_W))" ] &&
    [ "$PRESS_Y" -ge "$TITLE_TOP" ] &&
    [ "$PRESS_Y" -lt "$TITLE_BOTTOM" ] &&
    [ "$PRESS_Y" -lt "$META_TOP" ] ||
    fail "drag press ($PRESS_X,$PRESS_Y) is outside title bounds or inside metadata"
DONE_LEFT=$((16 + 4 * LANE_W))
xdotool mousemove --sync --window "$WID" "$PRESS_X" "$PRESS_Y"
sleep 0.2
import -window "$WID" /output/beads-board-drag-base.png
xdotool mousedown 1
ARM_X=$((PRESS_X + 3))
ARM_OFFSET_X=$((ARM_X - SOURCE_LEFT))
xdotool mousemove --sync --window "$WID" "$ARM_X" "$PRESS_Y"
sleep 0.1

# Same card geometry over the Done card: a substantial changed area proves the
# source card is painted above the existing lane content, not clipped into it.
OVER_X=$((DONE_LEFT + ARM_OFFSET_X))
OVER_Y=$((SOURCE_TOP + PRESS_OFFSET_Y))
xdotool mousemove --sync --window "$WID" "$OVER_X" "$OVER_Y"
sleep 0.25
import -window "$WID" /output/beads-board-drag-over-cards.png
OVER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-over-cards.png -compose difference -composite \
    -crop "${CARD_W}x${CARD_H}+${DONE_LEFT}+${SOURCE_TOP}" +repage \
    -colorspace Gray -threshold 5% -format "%[fx:mean*w*h]" info:)
awk -v changed="$OVER_CHANGED" 'BEGIN { exit !(changed >= 200) }' ||
    fail "drag ghost did not paint above the Done card (${OVER_CHANGED}px changed)"

# An accepting target gets a compact semantic border, not a lane-sized tint.
DONE_BORDER_X=$((8 + (4 * (WIN_W - 16) + 2) / 5))
DONE_BORDER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-over-cards.png -compose difference -composite \
    -crop "2x100+${DONE_BORDER_X}+120" +repage -colorspace Gray -threshold 5% \
    -format "%[fx:mean*w*h]" info:)
awk -v changed="$DONE_BORDER_CHANGED" 'BEGIN { exit !(changed >= 80) }' ||
    fail "Done target border changed only ${DONE_BORDER_CHANGED}px"
assert_drag_frame /output/beads-board-drag-over-cards.png \
    "$OVER_X" "$OVER_Y" over-card "$DONE_BORDER_X"

# Backlog is a rejected target. It gets a compact neutral border while the
# bare lower lane remains the same board ground.
BACKLOG_X=$((ARM_OFFSET_X + 20))
xdotool mousemove --sync --window "$WID" "$BACKLOG_X" "$PRESS_Y"
sleep 0.25
import -window "$WID" /output/beads-board-drag-no-drop.png
BACKLOG_BORDER_X=8
NO_DROP_BORDER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-no-drop.png -compose difference -composite \
    -crop "2x100+${BACKLOG_BORDER_X}+120" +repage -colorspace Gray -threshold 5% \
    -format "%[fx:mean*w*h]" info:)
awk -v changed="$NO_DROP_BORDER_CHANGED" 'BEGIN { exit !(changed >= 80) }' ||
    fail "Backlog no-drop border changed only ${NO_DROP_BORDER_CHANGED}px"
assert_drag_frame /output/beads-board-drag-no-drop.png \
    "$BACKLOG_X" "$PRESS_Y" no-drop "$BACKLOG_BORDER_X"
convert /output/beads-board-drag-base.png \
    -crop "${LANE_W}x8+8+220" +repage /tmp/beads-ground-before.png
convert /output/beads-board-drag-no-drop.png \
    -crop "${LANE_W}x8+8+220" +repage /tmp/beads-ground-no-drop.png
NO_DROP_GROUND_CHANGED=$(compare -metric AE /tmp/beads-ground-before.png \
    /tmp/beads-ground-no-drop.png null: 2>&1 || true)
NO_DROP_GROUND_CHANGED=${NO_DROP_GROUND_CHANGED%%.*}
[ "${NO_DROP_GROUND_CHANGED:-999}" -le 20 ] ||
    fail "Backlog no-drop tinted ${NO_DROP_GROUND_CHANGED}px of neutral ground"

# Stay outside the strip beyond its 150ms hover grace. The board must remain
# open for the gesture, the terminal geometry must stay fixed, and the ghost's
# opaque bounds must land within 3px of pointer minus the source press offset.
OUT_X=$((WIN_W / 2))
OUT_Y=320
xdotool mousemove --sync --window "$WID" "$OUT_X" "$OUT_Y"
sleep 0.5
import -window "$WID" /output/beads-board-drag-outside.png
DRAG_GROUND=$(convert /output/beads-board-drag-outside.png \
    -format "%[pixel:p{4,68}]" info:)
[ "$DRAG_GROUND" = "$BOARD_GROUND" ] || fail "hover board closed during card drag"
assert_drag_frame /output/beads-board-drag-outside.png "$OUT_X" "$OUT_Y" outside
xdotool mouseup 1

# The release lets the hover overlay close; reopen it for the remaining board
# controls, which continue from the same baseline.
xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
sleep 0.5

if [ "${SCRIBE_E2E_TOOLTIP_ONLY:-0}" = "1" ]; then
    echo "PASS: full-title tooltip ${TOOLTIP_W}x${TOOLTIP_H}+${TOOLTIP_X}+${TOOLTIP_Y}; copy=$COPIED; drag preserved rows=$BASE_ROWS"
    exit 0
fi

# The board's own text-size control: the buttons sit in the strip's top right,
# plus on the left and minus on its right. Compare only the grid band between
# the 34px titlebar and the 24px status bar: the titlebar's own badge focus
# repaint is independent of the board font, and the status bar's live CPU/MEM
# sparkline redraws every frame regardless of the board, so both a genuine
# 34px top and a genuine 24px bottom must stay out of this proof.
BOARD_BODY_CROP="${WIN_W}x$((WIN_H - 34 - 24))+0+34"
convert /output/beads-board-hover.png -crop "$BOARD_BODY_CROP" +repage \
    /tmp/beads-board-hover-body.png
xdotool mousemove --sync --window "$WID" "$((WIN_W - 32))" 49
sleep 0.3
xdotool click 1
sleep 0.6
import -window "$WID" /output/beads-board-larger.png
convert /output/beads-board-larger.png -crop "$BOARD_BODY_CROP" +repage \
    /tmp/beads-board-larger-body.png
LARGER_DIFF=$(compare -metric AE /tmp/beads-board-hover-body.png \
    /tmp/beads-board-larger-body.png null: 2>&1 || true)
LARGER_DIFF=${LARGER_DIFF%%.*}
[ "${LARGER_DIFF:-0}" -ge 2000 ] || fail "the larger-text button changed only ${LARGER_DIFF}px"
xdotool mousemove --sync --window "$WID" "$((WIN_W - 14))" 49
sleep 0.3
xdotool click 1
sleep 0.6
# Back to the bead before capturing: the baseline was taken with the pointer
# there, and a button under the pointer wears its hover fill.
xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
sleep 0.5
import -window "$WID" /output/beads-board-restored.png
convert /output/beads-board-restored.png -crop "$BOARD_BODY_CROP" +repage \
    /tmp/beads-board-restored-body.png
RESTORED_DIFF=$(compare -metric AE /tmp/beads-board-hover-body.png \
    /tmp/beads-board-restored-body.png null: 2>&1 || true)
RESTORED_DIFF=${RESTORED_DIFF%%.*}
[ "${RESTORED_DIFF:-9999}" -le 400 ] ||
    fail "the smaller-text button did not undo the larger one (${RESTORED_DIFF}px left over)"

# Pin and resize while there is still one region. Later split fixtures must not
# change the geometry whose edge and row deltas this phase measures.
TOP_ROWS=$(latest_rows "$WORKSPACE")
xdotool click 1
for _ in $(seq 1 20); do
    PIN_ROWS=$(latest_rows "$WORKSPACE")
    [ "$PIN_ROWS" -lt "$TOP_ROWS" ] && break
    sleep 0.2
done
[ "${PIN_ROWS:-$TOP_ROWS}" -lt "$TOP_ROWS" ] ||
    fail "pin click did not reserve terminal rows ($TOP_ROWS -> ${PIN_ROWS:-$TOP_ROWS})"
xdotool mousemove --sync --window "$WID" "$((WIN_W - 20))" "$((WIN_H - 20))"
sleep 0.3
import -window "$WID" /output/beads-board-pinned.png

# The bottom bar is a resize grip: dragging it down takes rows from this
# region's terminal, which is the whole point of the strip being a reservation
# rather than an overlay. Dragging it back returns them exactly.
EDGE=$(board_bottom /output/beads-board-pinned.png)
[ "${EDGE:-999}" -lt 400 ] || fail "could not find the board's bottom bar (got ${EDGE})"

# Pinned boards use the same neutral lane ground as hover boards.
for lane in 0 1 2 3 4; do
    PINNED_LANE_GROUND=$(convert /output/beads-board-pinned.png \
        -format "%[pixel:p{$((8 + lane * LANE_W + 4)),$((EDGE - 8))}]" info:)
    [ "$PINNED_LANE_GROUND" = "$BOARD_GROUND" ] ||
        fail "pinned lane $lane ground $PINNED_LANE_GROUND is tinted instead of neutral $BOARD_GROUND"
done
xdotool mousemove --sync --window "$WID" 40 "$EDGE"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" 40 "$((EDGE + 60))"
sleep 0.4
xdotool mouseup 1
for _ in $(seq 1 20); do
    GROWN_ROWS=$(latest_rows)
    [ "$GROWN_ROWS" -lt "$PIN_ROWS" ] && break
    sleep 0.2
done
[ "${GROWN_ROWS:-$PIN_ROWS}" -lt "$PIN_ROWS" ] ||
    fail "dragging the bottom bar down reserved no rows (still $PIN_ROWS)"
import -window "$(window_id)" /output/beads-board-resized.png
GROWN_EDGE=$(board_bottom /output/beads-board-resized.png)
[ "$((GROWN_EDGE - EDGE))" -ge 50 ] ||
    fail "the board painted $((GROWN_EDGE - EDGE))px taller, not the 60px dragged"
xdotool mousemove --sync --window "$WID" 40 "$GROWN_EDGE"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" 40 "$EDGE"
sleep 0.4
xdotool mouseup 1
for _ in $(seq 1 20); do
    SHRUNK_ROWS=$(latest_rows)
    [ "$SHRUNK_ROWS" -eq "$PIN_ROWS" ] && break
    sleep 0.2
done
[ "${SHRUNK_ROWS:-0}" -eq "$PIN_ROWS" ] ||
    fail "dragging the bar back left $SHRUNK_ROWS rows instead of $PIN_ROWS"


# ── Flow view: the strip's second rendering ──────────────────────────────
# Sited here, while the board is pinned at its default height in a single
# full-width region, because every coordinate below is derived from that
# layout. It runs before the split for the same reason the lower-region badge
# runs last: a topology change would invalidate the geometry these probes pin.
#
# Every constant mirrors `crates/scribe-client/src/beads_flow.rs`. They are
# written as the same formulas the renderer uses, not as measured pitches, so
# a change to the node box or the gutter fails here instead of silently
# re-siting every probe.
BOARD_TOP=34
FLOW_BAND_H=34
FLOW_RULER_H=15
FLOW_GRAPH_H=139
FLOW_GRAPH_TOP=$((BOARD_TOP + FLOW_BAND_H + FLOW_RULER_H))
FLOW_HBAR_TOP=$((FLOW_GRAPH_TOP + FLOW_GRAPH_H))
FLOW_FLOOR_BOTTOM=$((FLOW_HBAR_TOP + 2 + 3))
FLOW_NODE_W=214
FLOW_NODE_H=24
FLOW_GUTTER=28
FLOW_ROW_GAP=10
FLOW_LEFT_PAD=30
FLOW_RANK_PITCH=$((FLOW_NODE_W + FLOW_GUTTER))
FLOW_ROW_PITCH=$((FLOW_NODE_H + FLOW_ROW_GAP))
FLOW_PROGRESS_W=150

# Left edge of a rank's node box, and the centre of the 8px dot inside it. The
# node carries 6px of padding before the dot, so the dot's centre is 10px in.
flow_node_x() { echo "$((FLOW_LEFT_PAD + $1 * FLOW_RANK_PITCH))"; }
flow_dot_x() { echo "$(($(flow_node_x "$1") + 10))"; }
# `centered_row_tops`: a rank's rows are centred in the graph band, so the dot
# centre of row `$2` of `$1` rows is the band top plus that offset plus half a
# node. Doubled throughout to keep the half-pixel the renderer actually uses.
flow_dot_y() {
    local rows=$1 row=$2 total2 first2
    total2=$((2 * rows * FLOW_NODE_H + 2 * (rows - 1) * FLOW_ROW_GAP))
    first2=$((2 * FLOW_GRAPH_H - total2))
    echo "$(((2 * FLOW_GRAPH_TOP + first2 / 2 + 2 * row * FLOW_ROW_PITCH + FLOW_NODE_H) / 2))"
}

FLOW_CARD_X=$((16 + LANE_W + 40))
FLOW_CARD_Y=$((70 + 50 + 20))
inject "$(flow_board "$WORKSPACE")"
import -window "$WID" /output/beads-flow-lanes.png

# Opening a card opens the panel and swaps the strip. The graph is injected
# with no sleep between: the pending fence the click opened is cleared by any
# non-Graph outcome, and the real bd-less server answers NotDetected on its
# own schedule.
xdotool mousemove --sync --window "$WID" "$FLOW_CARD_X" "$FLOW_CARD_Y"
xdotool click 1
scribe-test share-inject --control "$CONTROL" "$(flow_epic_graph "$WORKSPACE")"
sleep 1.0
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.4
import -window "$WID" /output/beads-flow.png
FLOW_ENTER_DIFF=$(compare -metric AE /output/beads-flow-lanes.png /output/beads-flow.png null: 2>&1 || true)
FLOW_ENTER_DIFF=${FLOW_ENTER_DIFF%%.*}
[ "${FLOW_ENTER_DIFF:-0}" -ge 10000 ] ||
    fail "opening a Flow-eligible card changed only ${FLOW_ENTER_DIFF}px"

FLOW_GROUND=$(px_at /output/beads-flow.png 5 "$((FLOW_GRAPH_TOP + 20))")
FLOW_BAND=$(px_at /output/beads-flow.png 5 "$((BOARD_TOP + 16))")
[ "$(px_delta "$FLOW_BAND" "$FLOW_GROUND")" -ge 12 ] ||
    fail "the Flow band ($FLOW_BAND) does not read apart from the graph ground ($FLOW_GROUND)"

# The band's own composition: a back control, the epic name, the tally, a
# progress bar of the documented width whose fill is 2 of 7, and the opened
# tag. The fill is measured as a fraction of the bar rather than a pixel count,
# so the assertion states the ratio the band is claiming.
FLOW_BAR_Y=$((BOARD_TOP + 15))
FLOW_BAR_SPAN=$(convert /output/beads-flow.png \
    -crop "${FLOW_PROGRESS_W}x1+184+${FLOW_BAR_Y}" +repage txt:- |
    python3 -c '
import re, sys
row = []
for line in sys.stdin:
    m = re.match(r"(\d+),\d+: \((\d+),(\d+),(\d+)", line)
    if m:
        row.append((int(m.group(1)), [int(m.group(i)) for i in (2, 3, 4)]))
row.sort()
track = row[-1][1]
fill = sum(1 for _, c in row if sum(abs(a - b) for a, b in zip(c, track)) > 20)
print(f"{fill},{len(row)}")
')
FLOW_BAR_FILL=${FLOW_BAR_SPAN%%,*}
FLOW_BAR_TOTAL=${FLOW_BAR_SPAN##*,}
[ "$FLOW_BAR_TOTAL" -eq "$FLOW_PROGRESS_W" ] ||
    fail "the Flow progress bar spans ${FLOW_BAR_TOTAL}px, not the ${FLOW_PROGRESS_W}px the band reserves"
# 2 closed of 7 is 42.9px of 150; allow a pixel either side of the rounding.
[ "$FLOW_BAR_FILL" -ge 41 ] && [ "$FLOW_BAR_FILL" -le 45 ] ||
    fail "the progress fill is ${FLOW_BAR_FILL}px, not the ~43px that 2 closed of 7 paints"
FLOW_BACK_INK=$(convert /output/beads-flow.png -crop "60x18+8+$((BOARD_TOP + 8))" +repage \
    -format "%[fx:maxima]" info:)
awk -v v="$FLOW_BACK_INK" 'BEGIN { exit !(v > 0.3) }' ||
    fail "the Flow band paints no back-to-lanes control"

# The ruler is semantic, not one label per rank: SHIPPED sits on rank 0, NOW on
# the cursor's rank, NEXT on the one after it. Rank 1 carries none, and
# asserting that absence is what stops this becoming "some ink exists near the
# top of the graph".
rank_label_ink() {
    convert /output/beads-flow.png \
        -crop "56x12+$(flow_node_x "$1")+$((BOARD_TOP + FLOW_BAND_H + 2))" +repage \
        -format "%[fx:maxima]" info:
}
for rank in 0 2 3; do
    awk -v v="$(rank_label_ink "$rank")" 'BEGIN { exit !(v > 0.3) }' ||
        fail "rank $rank carries no ruler label at x=$(flow_node_x "$rank")"
done
awk -v v="$(rank_label_ink 1)" 'BEGIN { exit !(v < 0.3) }' ||
    fail "rank 1 carries a ruler label; the ruler is meant to name only rank 0, the cursor rank and the one after"

# Node state treatments, all on interior ranks. A filled state paints its hue
# at the dot centre; a ring state leaves the ground there and carries its hue
# on the rim 4px out. Reading both points is what separates the two, and it is
# why "the dot is coloured" is not the assertion.
# Rows are centred within the band, so two ranks holding the same number of
# rows share their row centres: rank 2's probes sit at rank 1's y. The names
# stay distinct because each pairs with its own flow_dot_x. Give either rank a
# different row count and these must be computed separately again.
FLOW_R1_TOP=$(flow_dot_y 2 0)
FLOW_R1_BOT=$(flow_dot_y 2 1)
FLOW_R2_TOP=$FLOW_R1_TOP
FLOW_R2_BOT=$FLOW_R1_BOT
assert_ring_dot() {
    local label="$1" x="$2" y="$3" centre rim
    centre=$(px_at /output/beads-flow.png "$x" "$y")
    rim=$(px_at /output/beads-flow.png "$((x - 4))" "$y")
    [ "$(px_delta "$centre" "$FLOW_GROUND")" -le 12 ] ||
        fail "$label should be a ring but its centre ($centre) is filled"
    [ "$(px_delta "$rim" "$FLOW_GROUND")" -ge 60 ] ||
        fail "$label has no hue on its ring rim ($rim)"
}
assert_filled_dot() {
    local label="$1" x="$2" y="$3" centre
    centre=$(px_at /output/beads-flow.png "$x" "$y")
    [ "$(px_delta "$centre" "$FLOW_GROUND")" -ge 60 ] ||
        fail "$label should be a filled dot but its centre ($centre) reads as ground"
}
assert_ring_dot "fl-b (blocked, rank 1)" "$(flow_dot_x 1)" "$FLOW_R1_TOP"
assert_ring_dot "fl-c (ready, rank 1)" "$(flow_dot_x 1)" "$FLOW_R1_BOT"
assert_filled_dot "sc-94 (in progress, rank 2)" "$(flow_dot_x 2)" "$FLOW_R2_TOP"
assert_filled_dot "fl-e (done, rank 2)" "$(flow_dot_x 2)" "$FLOW_R2_BOT"
FLOW_BLOCKED_RIM=$(px_at /output/beads-flow.png "$(($(flow_dot_x 1) - 4))" "$FLOW_R1_TOP")
FLOW_READY_RIM=$(px_at /output/beads-flow.png "$(($(flow_dot_x 1) - 4))" "$FLOW_R1_BOT")
[ "$(px_delta "$FLOW_BLOCKED_RIM" "$FLOW_READY_RIM")" -ge 40 ] ||
    fail "blocked and ready paint the same rim ($FLOW_BLOCKED_RIM); the states are not distinguishable"

# Wire endpoints land on dot centres. Read in the gutter between rank 0 and
# rank 1: fl-a leaves at its own dot centre and the two arrivals land at their
# targets' dot centres, which is the property a 1px offset in the node box
# would break.
FLOW_GUTTER_X=$(($(flow_node_x 0) + FLOW_NODE_W + FLOW_GUTTER / 2))
FLOW_ARRIVE_X=$(($(flow_node_x 1) - 4))
FLOW_DEPART_Y=$(flow_dot_y 1 0)
for probe in "depart:$(($(flow_node_x 0) + FLOW_NODE_W + 4)):$FLOW_DEPART_Y" \
    "arrive-fl-b:${FLOW_ARRIVE_X}:${FLOW_R1_TOP}" \
    "arrive-fl-c:${FLOW_ARRIVE_X}:${FLOW_R1_BOT}"; do
    IFS=: read -r WLABEL WX WY <<<"$probe"
    WIRE_ON=$(px_at /output/beads-flow.png "$WX" "$WY")
    WIRE_OFF=$(px_at /output/beads-flow.png "$WX" "$((WY - 6))")
    [ "$(px_delta "$WIRE_ON" "$FLOW_GROUND")" -ge 25 ] ||
        fail "no wire at the $WLABEL dot centre (${WX},${WY}); read $WIRE_ON against ground $FLOW_GROUND"
    [ "$(px_delta "$WIRE_OFF" "$FLOW_GROUND")" -le 12 ] ||
        fail "the $WLABEL wire is not confined to the dot centre: (${WX},$((WY - 6))) reads $WIRE_OFF"
done

# The cursor is unique. sc-94 sits on rank 2 beside fl-e, so a probe that
# merely found "a styled node" would pass on either; asserting the sibling is
# untreated is what makes the uniqueness observable at all.
FLOW_CURSOR_X=$(flow_node_x 2)
FLOW_CURSOR_FILL_X=$((FLOW_CURSOR_X + FLOW_NODE_W - 28))
FLOW_CURSOR_KEYLINE=$(px_at /output/beads-flow.png "$FLOW_CURSOR_X" "$FLOW_R2_TOP")
FLOW_CURSOR_FILL=$(px_at /output/beads-flow.png "$FLOW_CURSOR_FILL_X" "$FLOW_R2_TOP")
[ "$(px_delta "$FLOW_CURSOR_KEYLINE" "$FLOW_GROUND")" -ge 100 ] ||
    fail "the cursor node has no keyline at its leading edge ($FLOW_CURSOR_KEYLINE)"
[ "$(px_delta "$FLOW_CURSOR_FILL" "$FLOW_GROUND")" -ge 8 ] ||
    fail "the cursor node carries no fill ($FLOW_CURSOR_FILL against ground $FLOW_GROUND)"
FLOW_SIBLING_KEYLINE=$(px_at /output/beads-flow.png "$FLOW_CURSOR_X" "$FLOW_R2_BOT")
FLOW_SIBLING_FILL=$(px_at /output/beads-flow.png "$FLOW_CURSOR_FILL_X" "$FLOW_R2_BOT")
[ "$(px_delta "$FLOW_SIBLING_KEYLINE" "$FLOW_GROUND")" -le 12 ] ||
    fail "fl-e shares rank 2 with the cursor and also wears a keyline ($FLOW_SIBLING_KEYLINE)"
[ "$(px_delta "$FLOW_SIBLING_FILL" "$FLOW_GROUND")" -le 8 ] ||
    fail "fl-e shares rank 2 with the cursor and also wears its fill ($FLOW_SIBLING_FILL)"

# Hover traces the chain. sc-94's ancestors are fl-b and fl-a and its
# descendants are fl-f and fl-g, so fl-c and fl-e are off-path. fl-a's two
# out-edges share one vertical gutter run: tracing sc-94 must light the half
# that reaches fl-b and dim the half that reaches fl-c. A router that emitted
# whole edges instead of interval-unioned segments cannot produce that split,
# which is the point of reading both halves of one run.
xdotool mousemove --sync --window "$WID" "$((FLOW_CURSOR_X + 90))" "$FLOW_R2_TOP"
sleep 0.6
import -window "$WID" /output/beads-flow-trace.png
FLOW_TRACED=$(px_at /output/beads-flow-trace.png "$FLOW_GUTTER_X" "$((FLOW_R1_TOP + 6))")
FLOW_DIMMED=$(px_at /output/beads-flow-trace.png "$FLOW_GUTTER_X" "$((FLOW_R1_BOT - 6))")
FLOW_WIRE_BASE=$(px_at /output/beads-flow.png "$FLOW_GUTTER_X" "$((FLOW_R1_TOP + 6))")
[ "$(px_delta "$FLOW_TRACED" "$FLOW_WIRE_BASE")" -ge 60 ] ||
    fail "the on-path half of the shared gutter did not brighten ($FLOW_WIRE_BASE -> $FLOW_TRACED)"
[ "$(px_delta "$FLOW_DIMMED" "$FLOW_WIRE_BASE")" -ge 15 ] ||
    fail "the off-path half of the shared gutter did not dim ($FLOW_WIRE_BASE -> $FLOW_DIMMED)"
[ "$(px_delta "$FLOW_TRACED" "$FLOW_DIMMED")" -ge 100 ] ||
    fail "one shared gutter run painted both halves alike (traced $FLOW_TRACED, dimmed $FLOW_DIMMED)"
FLOW_OFFPATH_RIM=$(px_at /output/beads-flow-trace.png "$(($(flow_dot_x 1) - 4))" "$FLOW_R1_BOT")
[ "$(px_delta "$FLOW_OFFPATH_RIM" "$FLOW_READY_RIM")" -ge 40 ] ||
    fail "off-path fl-c kept its full-strength rim under trace ($FLOW_OFFPATH_RIM)"

# The trace chip states the closure, which is deliberately not a direct-edge
# count: sc-94 has one direct dependent but releases two.
FLOW_CHIP_Y=$((FLOW_R2_TOP + FLOW_NODE_H + 6))
FLOW_CHIP_BOUNDS=$(convert /output/beads-flow.png /output/beads-flow-trace.png \
    -compose difference -composite -colorspace Gray \
    -crop "300x30+${FLOW_CURSOR_X}+${FLOW_CHIP_Y}" +repage \
    -threshold 10% -trim -format '%w,%h' info: 2>/dev/null || true)
FLOW_CHIP_W=${FLOW_CHIP_BOUNDS%%,*}
[ "${FLOW_CHIP_W:-0}" -ge 80 ] ||
    fail "hovering the cursor node revealed no trace chip (${FLOW_CHIP_BOUNDS:-empty})"
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.5
import -window "$WID" /output/beads-flow-restored.png
FLOW_RESTORE_DIFF=$(strip_diff /output/beads-flow.png /output/beads-flow-restored.png)
[ "${FLOW_RESTORE_DIFF:-9999}" -le 400 ] ||
    fail "leaving the hover left ${FLOW_RESTORE_DIFF}px of the strip changed"

# A live agent's halo. Injected as the focused-issue frame the server sends,
# which is the exact issue-to-session join the halo answers to — an assignee
# string alone must never light it. fl-c is `ready`, so a ring before and a
# filled core inside a halo after is the whole treatment change.
FLOW_LIVE_SESSION=$(python3 - "$RECORD" <<'FLOWPY'
import json, sys
found = ""
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if message.get("session_id"):
        found = message["session_id"]
print(found)
FLOWPY
)
[ -n "$FLOW_LIVE_SESSION" ] || fail "no session id on the wire to bind a focused issue to"
inject "{\"type\":\"IssueFocused\",\"session_id\":\"$FLOW_LIVE_SESSION\",\"issue_id\":\"fl-c\"}"
import -window "$WID" /output/beads-flow-live.png
FLOW_LIVE_CORE=$(px_at /output/beads-flow-live.png "$(flow_dot_x 1)" "$FLOW_R1_BOT")
FLOW_LIVE_HALO=$(px_at /output/beads-flow-live.png "$(($(flow_dot_x 1) - 6))" "$FLOW_R1_BOT")
[ "$(px_delta "$FLOW_LIVE_CORE" "$FLOW_GROUND")" -ge 60 ] ||
    fail "the live node's dot core did not fill ($FLOW_LIVE_CORE)"
[ "$(px_delta "$FLOW_LIVE_HALO" "$FLOW_GROUND")" -ge 15 ] ||
    fail "the live node carries no halo outside its dot ($FLOW_LIVE_HALO)"
FLOW_IDLE_HALO=$(px_at /output/beads-flow.png "$(($(flow_dot_x 1) - 6))" "$FLOW_R1_BOT")
[ "$(px_delta "$FLOW_IDLE_HALO" "$FLOW_GROUND")" -le 12 ] ||
    fail "an idle node already paints a halo ($FLOW_IDLE_HALO); the live treatment proves nothing"
FLOW_NOT_LIVE=$(px_at /output/beads-flow-live.png "$(($(flow_dot_x 1) - 6))" "$FLOW_R1_TOP")
[ "$(px_delta "$FLOW_NOT_LIVE" "$FLOW_GROUND")" -le 12 ] ||
    fail "fl-b is not the focused issue but wears a halo ($FLOW_NOT_LIVE)"

# The epic chevron is inert for now. Scoped to the strip: the detail panel
# under the board repaints on its own and would drown the assertion.
import -window "$WID" /output/beads-flow-prechevron.png
xdotool mousemove --sync --window "$WID" 138 "$((BOARD_TOP + 16))"
xdotool click 1
sleep 0.6
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.4
import -window "$WID" /output/beads-flow-chevron.png
FLOW_CHEVRON_DIFF=$(strip_diff /output/beads-flow-prechevron.png /output/beads-flow-chevron.png)
[ "${FLOW_CHEVRON_DIFF:-9999}" -le 200 ] ||
    fail "clicking the epic chevron changed ${FLOW_CHEVRON_DIFF}px of the strip; it is meant to be inert"

# The wheel scrolls the graph sideways, and no state grows a vertical bar.
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((FLOW_GRAPH_TOP + 40))"
xdotool click 5
xdotool click 5
xdotool click 5
sleep 0.6
import -window "$WID" /output/beads-flow-scrolled.png
FLOW_SCROLL_DIFF=$(strip_diff /output/beads-flow-live.png /output/beads-flow-scrolled.png)
[ "${FLOW_SCROLL_DIFF:-0}" -ge 500 ] ||
    fail "the wheel moved the Flow graph by only ${FLOW_SCROLL_DIFF}px of strip"
for state in /output/beads-flow.png /output/beads-flow-trace.png \
    /output/beads-flow-live.png /output/beads-flow-scrolled.png; do
    FLOW_VBAR=$(right_edge_run "$state")
    [ "${FLOW_VBAR:-999}" -le 40 ] ||
        fail "$state grows a ${FLOW_VBAR}px vertical bar at the graph's right edge"
done
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((FLOW_GRAPH_TOP + 40))"
xdotool click 4
xdotool click 4
xdotool click 4
xdotool click 4
sleep 0.5
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.4

# Every rendered Flow colour comes from the theme. This is the assertion the
# board's colour rule actually needs: a hardcoded value survives every probe
# above and fails only here, because a theme edit has to move all of them.
# Sampled under hover and with the halo lit, so the traced, dimmed, chip and
# agent slots are covered alongside the static ones.
# Re-establish the whole Flow state from scratch.
#
# The real bd-less server answers NotDetected on its own schedule, and that
# reply legitimately tears the board down — it clears the pin and leaves Flow.
# Any wait longer than a second or two can therefore lose the strip, so every
# phase that has to wait (both theme reloads below) rebuilds the state here
# instead of assuming it survived.
flow_enter() {
    inject "$(flow_board "$WORKSPACE")"
    if [ "$(latest_rows "$WORKSPACE")" -ge "$TOP_ROWS" ]; then
        xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
        sleep 0.4
        xdotool click 1
        sleep 0.8
        inject "$(flow_board "$WORKSPACE")"
    fi
    xdotool mousemove --sync --window "$WID" "$FLOW_CARD_X" "$FLOW_CARD_Y"
    xdotool click 1
    scribe-test share-inject --control "$CONTROL" "$(flow_epic_graph "$WORKSPACE")"
    sleep 1.0
    scribe-test share-inject --control "$CONTROL" \
        "{\"type\":\"IssueFocused\",\"session_id\":\"$FLOW_LIVE_SESSION\",\"issue_id\":\"fl-c\"}"
    sleep 0.6
}

FLOW_CONFIG_DIR="$XDG_CONFIG_HOME/scribe"
FLOW_CONFIG_FILE="$FLOW_CONFIG_DIR/config.toml"
mkdir -p "$FLOW_CONFIG_DIR"
flow_slot_sites() {
    printf '%s\n' \
        "wire:${FLOW_GUTTER_X}:$((FLOW_R1_BOT - 6))" \
        "wire_traced:${FLOW_GUTTER_X}:$((FLOW_R1_TOP + 6))" \
        "band:5:$((BOARD_TOP + 16))" \
        "progress_track:$((184 + FLOW_PROGRESS_W - 8)):${FLOW_BAR_Y}" \
        "cursor_keyline:${FLOW_CURSOR_X}:${FLOW_R2_TOP}" \
        "cursor_fill:${FLOW_CURSOR_FILL_X}:${FLOW_R2_TOP}" \
        "rank_label:$(flow_node_x 2):$((BOARD_TOP + FLOW_BAND_H + 4))" \
        "agent_halo:$(($(flow_dot_x 1) - 6)):${FLOW_R1_BOT}"
}
flow_hover_cursor() {
    xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
    sleep 0.3
    xdotool mousemove --sync --window "$WID" "$((FLOW_CURSOR_X + 90))" "$FLOW_R2_TOP"
    sleep 0.7
}
flow_enter
flow_hover_cursor
import -window "$WID" /output/beads-flow-theme-before.png
FLOW_RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
printf '[appearance]\ntheme = "dracula"\n' >"$FLOW_CONFIG_FILE"
for _ in $(seq 1 40); do
    [ "$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)" -gt "${FLOW_RELOADS_BEFORE:-0}" ] && break
    sleep 0.5
done
[ "$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)" -gt "${FLOW_RELOADS_BEFORE:-0}" ] ||
    fail "the client never hot-reloaded the rewritten theme"
sleep 1.0
flow_enter
flow_hover_cursor
import -window "$WID" /output/beads-flow-theme-after.png
FLOW_STATIC_SLOTS=""
while IFS=: read -r SLOT SX SY; do
    BEFORE=$(px_at /output/beads-flow-theme-before.png "$SX" "$SY")
    AFTER=$(px_at /output/beads-flow-theme-after.png "$SX" "$SY")
    if [ "$(px_delta "$BEFORE" "$AFTER")" -lt 10 ]; then
        FLOW_STATIC_SLOTS="$FLOW_STATIC_SLOTS $SLOT($BEFORE)"
    fi
done <<<"$(flow_slot_sites)"
[ -z "$FLOW_STATIC_SLOTS" ] ||
    fail "these Flow colours did not move with the theme, so they are not theme-derived:$FLOW_STATIC_SLOTS"

# Put the theme back before anything downstream reads a colour, and prove it
# landed rather than assuming it: every later phase compares against the board
# ground captured under the original theme. Written explicitly as the default
# name rather than deleted — removing the file leaves the client on the last
# config it parsed, so the board stayed dracula.
printf '[appearance]\ntheme = "minimal-dark"\n' >"$FLOW_CONFIG_FILE"
FLOW_RELOADS_RESTORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
for _ in $(seq 1 40); do
    [ "$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)" -gt "$FLOW_RELOADS_RESTORE" ] && break
    sleep 0.5
done
sleep 1.0
flow_enter
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.5
import -window "$WID" /output/beads-flow-theme-restored.png
FLOW_RESTORED_BAND=$(px_at /output/beads-flow-theme-restored.png 5 "$((BOARD_TOP + 16))")
[ "$(px_delta "$FLOW_RESTORED_BAND" "$FLOW_BAND")" -le 10 ] ||
    fail "the theme did not return to $FLOW_BAND (still $FLOW_RESTORED_BAND); later phases would read the wrong ground"

# Back to lanes, and the strip is a board again. The pin survives the round
# trip: Flow borrows the reservation, it does not own it.
#
# Esc is the exit, not the band's "← LANES" text: that label is a plain div
# with no click handler, as are the mode pair and the chevron — the only
# pointer handler in beads_flow.rs is on a node. Asserting a click on the label
# would pin behaviour the renderer does not have. A node is clicked first so
# focus sits in the strip, because Esc yields to the detail panel when the
# panel holds it.
flow_enter
import -window "$WID" /output/beads-flow-inflow.png
[ "$(strip_diff /output/beads-flow-lanes.png /output/beads-flow-inflow.png)" -ge 500 ] ||
    fail "could not re-enter Flow to prove the way back out"
FLOW_ROWS_IN_FLOW=$(latest_rows "$WORKSPACE")
xdotool mousemove --sync --window "$WID" "$(($(flow_node_x 1) + 90))" "$FLOW_R1_TOP"
xdotool click 1
sleep 0.5
xdotool key --clearmodifiers Escape
sleep 0.8
xdotool mousemove --sync --window "$WID" "$((WIN_W / 2))" "$((WIN_H - 80))"
sleep 0.4
import -window "$WID" /output/beads-flow-back.png
FLOW_BACK_DIFF=$(strip_diff /output/beads-flow-inflow.png /output/beads-flow-back.png)
[ "${FLOW_BACK_DIFF:-0}" -ge 500 ] ||
    fail "Escape did not return the strip to lanes (${FLOW_BACK_DIFF}px changed)"
[ "$(latest_rows "$WORKSPACE")" -eq "$FLOW_ROWS_IN_FLOW" ] ||
    fail "returning to lanes changed the pinned reservation ($FLOW_ROWS_IN_FLOW -> $(latest_rows "$WORKSPACE"))"
inject "$(sample_board "$WORKSPACE")"

# A pinned board is a citizen of its own region, not a window-wide band. The
# split re-parks WorkspaceInfo for the source region too, which re-asks the
# real server for its board; the isolated container has no bd project to find,
# so the real server genuinely answers NotDetected on its own — this is not
# simulated. Wait for that real reply on the wire before touching the
# workspace again: installing the controlled snapshot first would race an
# in-flight real reply that can land after and silently clear the board this
# proof depends on. share-tap relays real and injected messages through the
# same single channel in arrival order, so once the real NotDetected is
# recorded, every later `inject` is guaranteed to reach the client after it.
PINNED_WS="ws-${WORKSPACE:0:8}"
SPLIT_RECORD_MARK=$(record_mark)
xdotool key --clearmodifiers ctrl+alt+backslash
for _ in $(seq 1 40); do
    OTHER_WS=$(published_workspaces | tr ' ' '\n' | grep -v "^${PINNED_WS}\$" | tail -1 || true)
    [ -n "$OTHER_WS" ] && [ "$(latest_rows "$OTHER_WS")" -gt 0 ] && break
    sleep 0.3
done
[ -n "${OTHER_WS:-}" ] || fail "the workspace split never published a second region"
for _ in $(seq 1 40); do
    [ "$(server_reported_not_detected "$WORKSPACE" "$SPLIT_RECORD_MARK")" = "1" ] && break
    sleep 0.2
done
[ "$(server_reported_not_detected "$WORKSPACE" "$SPLIT_RECORD_MARK")" = "1" ] ||
    fail "the real server never answered NotDetected for the split source"
SPLIT_BASE=$(latest_rows "$PINNED_WS")
SPLIT_OTHER=$(latest_rows "$OTHER_WS")
[ "$SPLIT_BASE" -gt 0 ] && [ "$SPLIT_OTHER" -gt 0 ] ||
    fail "the workspace split did not settle both region geometries"

# Re-arm the left region after its rootless transition, pin it from the
# positional titlebar badge, then focus the neighbour. Focus is not a pin input:
# the original region must keep the reservation while the other owns focus.
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":\"/work/scribe\"}"
inject "$(sample_board "$WORKSPACE")"
WID=$(window_id)
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
sleep 0.3
xdotool click 1
for _ in $(seq 1 20); do
    SPLIT_PINNED=$(latest_rows "$PINNED_WS")
    [ "$SPLIT_PINNED" -lt "$SPLIT_BASE" ] && break
    sleep 0.2
done
[ "${SPLIT_PINNED:-$SPLIT_BASE}" -lt "$SPLIT_BASE" ] ||
    fail "the split region did not reserve rows when pinned"
xdotool mousemove --sync --window "$WID" "$((3 * WIDTH / 4))" "$((HEIGHT / 2))"
xdotool click 1
sleep 0.3
SPLIT_PINNED=$(latest_rows "$PINNED_WS")
SPLIT_OTHER=$(latest_rows "$OTHER_WS")
import -window "$WID" /output/beads-board-split.png
[ "$SPLIT_PINNED" -lt "$SPLIT_OTHER" ] ||
    fail "board reserved rows outside its region (pinned $SPLIT_PINNED, other $SPLIT_OTHER)"

# Regions are independent, not exclusive: give the second region a board of its
# own and pin it too. Both must then be open, each holding its own rows.
SECOND=$(other_workspace "$WORKSPACE") || fail "the split produced no second workspace"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$SECOND\",\"name\":\"beads\",\"accent_color\":\"#22d3ee\",\"split_direction\":null,\"project_root\":null}"
inject "$(sample_board "$SECOND")"
WID=$(window_id)
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove --sync --window "$WID" "$((WIDTH / 2 + TOP_BADGE_ICON_X))" "$TOP_BADGE_ICON_Y"
sleep 0.5
xdotool click 1
for _ in $(seq 1 20); do
    BOTH_OTHER=$(latest_rows "$OTHER_WS")
    [ "$BOTH_OTHER" -lt "$SPLIT_OTHER" ] && break
    sleep 0.3
done
import -window "$(window_id)" /output/beads-board-both.png
# The region divider runs the full height of the split, so it must still
# separate the two boards rather than being painted over by them. It is a hair
# under a pixel wide, so this samples a band around the split instead of the
# exact column, and asks only that the band is not uniform board background.
BOARD_Y=120
DIVIDER_BAND=$(convert /output/beads-board-both.png \
    -crop "9x1+$((WIDTH / 2 - 4))+${BOARD_Y}" +repage -format %k info:)
[ "${DIVIDER_BAND:-1}" -gt 1 ] ||
    fail "no region divider between two open boards at y=${BOARD_Y}"
BOTH_PINNED=$(latest_rows "$PINNED_WS")
[ "${BOTH_OTHER:-$SPLIT_OTHER}" -lt "$SPLIT_OTHER" ] ||
    fail "pinning the second region's board reserved nothing (still $SPLIT_OTHER rows)"
[ "$BOTH_PINNED" -eq "$SPLIT_PINNED" ] ||
    fail "the second board disturbed the first region ($SPLIT_PINNED -> $BOTH_PINNED rows)"

# A queue that has run dry says so where its first card would have been, rather
# than leaving its head floating over a void. Sampled in that slot: bare ground
# is one colour, and the ghost's dashed outline and its word are not.
inject "$(empty_board "$WORKSPACE")"
import -window "$(window_id)" /output/beads-board-empty.png
# The first card's slot in the left region's first lane, taken across the middle
# third of the lane. One colour means the empty-state ghost is missing.
LANE_W=$(((WIDTH / 2 - 16) / 5))
EMPTY_SLOT=$(convert /output/beads-board-empty.png \
    -crop "$((LANE_W / 3))x20+$((8 + LANE_W / 3))+86" +repage -format %k info:)
[ "${EMPTY_SLOT:-1}" -gt 1 ] ||
    fail "an empty queue left its lane blank instead of saying so"

# A workspace usually gains its root from CWD naming, which lands after the
# SessionList that seeds the eager requests. Both gaining and losing that root
# must ask again, and either answer may retire only this workspace's board.
ROOTED_REQUESTS=$(board_request_count "$WORKSPACE")
inject "{\"type\":\"WorkspaceNamed\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"project_root\":\"/work/scribe\"}"
[ "$(board_request_count "$WORKSPACE")" -gt "$ROOTED_REQUESTS" ] ||
    fail "naming a rooted workspace did not request its board"

inject "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$WORKSPACE\",\"protocol_version\":1,\"state\":\"NotDetected\"}"
for _ in $(seq 1 20); do
    ROOTED_NOT_DETECTED_ROWS=$(latest_rows "$PINNED_WS")
    [ "$ROOTED_NOT_DETECTED_ROWS" -gt "$BOTH_PINNED" ] && break
    sleep 0.2
done
[ "${ROOTED_NOT_DETECTED_ROWS:-$BOTH_PINNED}" -gt "$BOTH_PINNED" ] ||
    fail "NotDetected left the newly rooted workspace's pinned board open"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] ||
    fail "rooted NotDetected disturbed the neighbouring workspace's board"
import -window "$(window_id)" /output/beads-board-rooted-not-detected.png

# Restore the snapshot and pin it again so root loss proves the same cleanup
# independently rather than passing on the board the rooted case closed.
inject "$(sample_board "$WORKSPACE")"
xdotool mousemove --sync --window "$WID" "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y"
sleep 0.3
xdotool click 1
for _ in $(seq 1 20); do
    REPINNED_ROWS=$(latest_rows "$PINNED_WS")
    [ "$REPINNED_ROWS" -lt "$ROOTED_NOT_DETECTED_ROWS" ] && break
    sleep 0.2
done
[ "${REPINNED_ROWS:-$ROOTED_NOT_DETECTED_ROWS}" -lt "$ROOTED_NOT_DETECTED_ROWS" ] ||
    fail "restored board did not repin before the rootless transition"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] ||
    fail "repinning the restored board disturbed its neighbour"

ROOTLESS_REQUESTS=$(board_request_count "$WORKSPACE")
inject "{\"type\":\"WorkspaceNamed\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"\",\"project_root\":null}"
[ "$(board_request_count "$WORKSPACE")" -gt "$ROOTLESS_REQUESTS" ] ||
    fail "clearing a workspace root did not request its board"

inject "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$WORKSPACE\",\"protocol_version\":1,\"state\":\"NotDetected\"}"
for _ in $(seq 1 20); do
    ROOTLESS_NOT_DETECTED_ROWS=$(latest_rows "$PINNED_WS")
    [ "$ROOTLESS_NOT_DETECTED_ROWS" -gt "$REPINNED_ROWS" ] && break
    sleep 0.2
done
[ "${ROOTLESS_NOT_DETECTED_ROWS:-$REPINNED_ROWS}" -gt "$REPINNED_ROWS" ] ||
    fail "NotDetected left the newly rootless workspace's pinned board open"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] ||
    fail "rootless NotDetected disturbed the neighbouring workspace's board"
import -window "$(window_id)" /output/beads-board-rootless-not-detected.png

# The lower-region badge is a separate render path from the titlebar badge.
# Exercise it last so its stacked topology cannot disturb the single-region
# resize or the side-by-side reservation proofs above.
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":\"/work/scribe\"}"
inject "$(sample_board "$WORKSPACE")"
WID=$(window_id)
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove --sync --window "$WID" "$((WIDTH / 4))" "$((HEIGHT / 2))"
xdotool click 1
sleep 0.3
import -window "$WID" /output/beads-board-lower-titlebar.png
BEFORE_LOWER_SPLIT=" $(server_workspaces) "
xdotool key --clearmodifiers ctrl+alt+minus
for _ in $(seq 1 20); do
    LOWER_WS=""
    for candidate in $(server_workspaces); do
        case "$BEFORE_LOWER_SPLIT" in
        *" $candidate "*) ;;
        *) LOWER_WS="$candidate" ;;
        esac
    done
    [ -n "$LOWER_WS" ] && break
    sleep 0.3
done
[ -n "${LOWER_WS:-}" ] || fail "stacked split created no lower workspace"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$LOWER_WS\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":\"/work/scribe\"}"
inject "$(sample_board "$LOWER_WS")"
focus

GRID_HEIGHT=$((HEIGHT - 34 - 24))
LOWER_BADGE_ICON_X=13
LOWER_BADGE_ICON_Y=$((34 + GRID_HEIGHT / 2 + 17))
LOWER_BADGE_LABEL_X=44
import -window "$WID" /output/beads-board-lower-badge.png
badge_mark /output/beads-board-lower-titlebar.png \
    "$TOP_BADGE_ICON_X" "$TOP_BADGE_ICON_Y" /tmp/beads-titlebar-mark.png
badge_mark /output/beads-board-lower-badge.png \
    "$LOWER_BADGE_ICON_X" "$LOWER_BADGE_ICON_Y" /tmp/beads-region-mark.png
assert_matching_badge_marks /tmp/beads-titlebar-mark.png /tmp/beads-region-mark.png

LOWER_ROWS=$(latest_rows "$LOWER_WS")
xdotool mousemove --sync --window "$WID" "$LOWER_BADGE_LABEL_X" "$LOWER_BADGE_ICON_Y"
xdotool click 1
sleep 0.3
[ "$(latest_rows "$LOWER_WS")" -eq "$LOWER_ROWS" ] ||
    fail "lower workspace label changed its Beads reservation"
import -window "$WID" /output/beads-board-lower-before-hover.png
xdotool mousemove --sync --window "$WID" "$LOWER_BADGE_ICON_X" "$LOWER_BADGE_ICON_Y"
sleep 0.5
import -window "$WID" /output/beads-board-lower-hover.png
LOWER_HOVER_DIFF=$(compare -metric AE /output/beads-board-lower-before-hover.png \
    /output/beads-board-lower-hover.png null: 2>&1 || true)
LOWER_HOVER_DIFF=${LOWER_HOVER_DIFF%%.*}
[ "${LOWER_HOVER_DIFF:-0}" -ge 10000 ] ||
    fail "lower Beads icon hover changed only ${LOWER_HOVER_DIFF}px"
xdotool click 1
for _ in $(seq 1 20); do
    LOWER_PINNED_ROWS=$(latest_rows "$LOWER_WS")
    [ "$LOWER_PINNED_ROWS" -lt "$LOWER_ROWS" ] && break
    sleep 0.2
done
[ "${LOWER_PINNED_ROWS:-$LOWER_ROWS}" -lt "$LOWER_ROWS" ] ||
    fail "lower Beads icon click did not pin its board"

echo "PASS: Beads Constellation rendered for $WORKSPACE; pin rows $BASE_ROWS -> $PIN_ROWS;" \
    "bar drag $EDGE -> $GROWN_EDGE reserved $PIN_ROWS -> $GROWN_ROWS rows;" \
    "split pinned=$SPLIT_PINNED other=$SPLIT_OTHER; both pinned=$BOTH_PINNED other=$BOTH_OTHER;" \
    "rooted NotDetected rows=$ROOTED_NOT_DETECTED_ROWS;" \
    "rootless NotDetected rows=$ROOTLESS_NOT_DETECTED_ROWS;" \
    "lower badge rows $LOWER_ROWS -> $LOWER_PINNED_ROWS;" \
    "flow entry ${FLOW_ENTER_DIFF}px, progress ${FLOW_BAR_FILL}/${FLOW_BAR_TOTAL},"  \
    "chip ${FLOW_CHIP_W}px, scroll ${FLOW_SCROLL_DIFF}px, chevron ${FLOW_CHEVRON_DIFF}px,"  \
    "every flow colour moved with the theme, escape back to lanes ${FLOW_BACK_DIFF}px"
