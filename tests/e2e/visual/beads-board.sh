#!/bin/bash
# e2e-timeout: 180
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
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
    printf '%s' "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$workspace\",\"protocol_version\":1,\"state\":{\"Ready\":{\"snapshot\":{\"refreshed_at_epoch_ms\":$now,\"backlog\":[{\"id\":\"sc-70\",\"title\":\"Document cache policy\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-81\",\"title\":\"Parse custom statuses\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-76\",\"title\":\"Expose stale timestamp\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-64\",\"title\":\"Polish empty queue copy\",\"priority\":4,\"blocker_ids\":[],\"parent_epic_name\":null}],\"ready\":[{\"id\":\"sc-88\",\"title\":\"$LONG_TITLE\",\"priority\":0,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-94\",\"title\":\"Cache stale state\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-32\",\"title\":\"Wire workspace refresh\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Workspace intelligence and rooted boards\"},{\"id\":\"sc-27\",\"title\":\"Add unavailable state\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null}],\"in_progress\":[{\"id\":\"sc-1bf\",\"title\":\"Workspace Beads board\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":\"Workspace intelligence and rooted boards\"},{\"id\":\"sc-43\",\"title\":\"Render queue rail\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-55\",\"title\":\"Pin board across tabs\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":\"GPUI client rebuild\"},{\"id\":\"sc-49\",\"title\":\"Preserve lane scroll\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":null}],\"blocked\":[{\"id\":\"sc-91\",\"title\":\"Sync conflict handling\",\"priority\":1,\"blocker_ids\":[\"sc-12\",\"sc-19\"],\"parent_epic_name\":null},{\"id\":\"sc-58\",\"title\":\"Remote workspace reads\",\"priority\":2,\"blocker_ids\":[\"sc-22\"],\"parent_epic_name\":null},{\"id\":\"sc-36\",\"title\":\"Restore cached snapshot\",\"priority\":3,\"blocker_ids\":[\"sc-08\"],\"parent_epic_name\":null},{\"id\":\"sc-18\",\"title\":\"Resolve Dolt lock state\",\"priority\":2,\"blocker_ids\":[\"sc-07\"],\"parent_epic_name\":null}],\"done\":[{\"id\":\"sc-61\",\"title\":\"$LONG_TITLE\",\"priority\":1,\"blocker_ids\":[],\"parent_epic_name\":\"GPUI client rebuild\"},{\"id\":\"sc-24\",\"title\":\"Detect workspace root\",\"priority\":2,\"blocker_ids\":[],\"parent_epic_name\":null},{\"id\":\"sc-16\",\"title\":\"Cache bd availability\",\"priority\":3,\"blocker_ids\":[],\"parent_epic_name\":\"Beads integration\"},{\"id\":\"sc-09\",\"title\":\"Tune titlebar spacing\",\"priority\":4,\"blocker_ids\":[],\"parent_epic_name\":null}],\"backlog_total\":12,\"ready_total\":4,\"in_progress_total\":5,\"blocked_total\":4,\"done_total\":38},\"stale\":false,\"refresh_error\":null}}}"
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
    convert "$1" -crop "${2}x${3}+0+${4}" +repage txt:- > "$dump"
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
    convert "$1" -crop "1x$(( WIN_H - 40 ))+4+40" +repage txt:- > "$dump"
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
        -format "%[pixel:p{$(( center_x - 8 )),$(( center_y - 8 ))}]" info:)
    convert "$image" -crop "16x16+$(( center_x - 8 ))+$(( center_y - 8 ))" \
        +repage -transparent "$background" -alpha extract "$output"
}

assert_matching_badge_marks() {
    local title_mark="$1" region_mark="$2" ink diff
    ink=$(identify -format '%[fx:mean*w*h]' "$title_mark")
    ink=${ink%%.*}
    [ "${ink:-0}" -eq 59 ] \
        || fail "titlebar Beads mark has ${ink:-0}px of foreground, expected 59px"
    diff=$(compare -metric AE "$title_mark" "$region_mark" null: 2>&1 || true)
    diff=${diff%%.*}
    [ "${diff:-1}" -eq 0 ] \
        || fail "titlebar and lower-region Beads marks differ by ${diff:-unknown}px"
}

# Measure one synchronized native-drag waypoint against pointer minus GPUI's
# threshold-crossing offset. ImageMagick reports the trimmed delta's `%X/%Y`.
assert_drag_frame() {
    local current="$1" pointer_x="$2" pointer_y="$3" label="$4" target_border_x="${5:-}"
    local expected_x expected_y crop_x crop_y border_crop_x bounds ghost_x ghost_y ghost_w ghost_h rows
    local -a target_mask=()
    expected_x=$(( pointer_x - ARM_OFFSET_X ))
    expected_y=$(( pointer_y - PRESS_OFFSET_Y ))
    crop_x=$(( expected_x - 8 ))
    crop_y=$(( expected_y - 8 ))
    # Target borders are asserted independently at their waypoints. Remove
    # only that known two-pixel raster footprint from the ghost's delta.
    if [ -n "$target_border_x" ]; then
        border_crop_x=$(( target_border_x - crop_x ))
        target_mask=(-fill black -draw "rectangle ${border_crop_x},0 $(( border_crop_x + 1 )),$(( CARD_H + 15 ))")
    fi
    bounds=$(convert /output/beads-board-drag-base.png "$current" \
        -compose difference -composite \
        -crop "$(( CARD_W + 16 ))x$(( CARD_H + 16 ))+${crop_x}+${crop_y}" +repage \
        -colorspace Gray -threshold 12% "${target_mask[@]}" \
        -trim -format '%X,%Y,%w,%h' info:)
    if [[ "$bounds" =~ ^\+([0-9]+),\+([0-9]+),([0-9]+),([0-9]+)$ ]]; then
        ghost_x=$(( crop_x + BASH_REMATCH[1] ))
        ghost_y=$(( crop_y + BASH_REMATCH[2] ))
        ghost_w=${BASH_REMATCH[3]}
        ghost_h=${BASH_REMATCH[4]}
    else
        fail "could not measure $label drag ghost (${bounds:-empty})"
    fi
    [ "$(( ghost_x - expected_x ))" -ge -3 ] \
        && [ "$(( ghost_x - expected_x ))" -le 3 ] \
        && [ "$(( ghost_y - expected_y ))" -ge -3 ] \
        && [ "$(( ghost_y - expected_y ))" -le 3 ] \
        && [ "$ghost_w" -ge "$(( CARD_W - 6 ))" ] \
        && [ "$ghost_h" -ge "$(( CARD_H - 6 ))" ] \
        || fail "$label ghost ${ghost_w}x${ghost_h}+${ghost_x}+${ghost_y}, expected ${CARD_W}x${CARD_H}+${expected_x}+${expected_y} within 3px"
    rows=$(latest_rows)
    [ "$rows" -eq "$BASE_ROWS" ] \
        || fail "$label drag changed terminal rows ($BASE_ROWS -> $rows)"
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
    -format "%[pixel:p{$(( WIN_W - 200 )),15}]" info:)
[ "$BOARD_GROUND" = "$CHROME_GROUND" ] \
    || fail "board ground $BOARD_GROUND is not the chrome's $CHROME_GROUND"

# The board stays neutral between the compact foreground marks. Every lane's
# bare lower area must match the board ground instead of carrying its queue
# colour as a full-height wash.
LANE_W=$(( (WIN_W - 16) / 5 ))
for lane in 0 1 2 3 4; do
    LANE_GROUND=$(convert /output/beads-board-hover.png \
        -format "%[pixel:p{$(( 8 + lane * LANE_W + 4 )),220}]" info:)
    [ "$LANE_GROUND" = "$BOARD_GROUND" ] \
        || fail "lane $lane ground $LANE_GROUND is tinted instead of neutral $BOARD_GROUND"
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
    'BEGIN { exit !(card > ground + 0.02) }' \
    || fail "the card fill ($CARD_FILL) does not sit above the ground ($GROUND_FILL)"

# A hotter priority wears a stronger badge. Read down the Ready lane's first
# three cards, which run P0, P1, P2 — the ranking has to hold across their
# three different hues, which is what the solved tint is for.
P0_EDGE=$(edge_delta /output/beads-board-hover.png 225 84)
P1_EDGE=$(edge_delta /output/beads-board-hover.png 225 134)
P2_EDGE=$(edge_delta /output/beads-board-hover.png 225 184)
# Compared as a ratio, not a margin: the ranks are a fifth apart by design, so
# a fixed number of levels would either pass a flat ramp or fail a fine one.
[ "$(( ${P0_EDGE:-0} * 100 ))" -gt "$(( P1_EDGE * 115 ))" ] \
    || fail "the P0 badge (${P0_EDGE}) does not outrank P1's (${P1_EDGE})"
[ "$(( ${P1_EDGE:-0} * 100 ))" -gt "$(( P2_EDGE * 115 ))" ] \
    || fail "the P1 badge (${P1_EDGE}) does not outrank P2's (${P2_EDGE})"

# The epic sits on the card's right edge. Measured on the id-and-epic line of
# the first card in each lane, three of which carry one.
EPIC_GAP=$(epic_right_gap /output/beads-board-hover.png "$WIN_W" 10 97)
[ "${EPIC_GAP:-999}" -le 6 ] || fail "the epic is ${EPIC_GAP}px short of the card's right edge"

# A truncated normal card reveals its complete fixture title immediately. The
# long Done card sits against the right viewport edge, so the wrapped popup also
# proves GPUI clamps the bounded tooltip inside the window. Mask the card's own
# hover repaint before measuring the popup.
TITLE_CARD_LEFT=$(( 16 + 4 * LANE_W ))
TITLE_CARD_TOP=70
TITLE_CARD_W=$(( LANE_W - 20 ))
TITLE_CARD_H=46
xdotool mousemove --sync --window "$WID" "$(( TITLE_CARD_LEFT + 90 ))" 84
sleep 0.1
import -window "$WID" /output/beads-board-title-tooltip.png
TOOLTIP_BOUNDS=$(convert /output/beads-board-hover.png \
    /output/beads-board-title-tooltip.png -compose difference -composite \
    -colorspace Gray -threshold 8% -fill black \
    -draw "rectangle 0,0 40,34 rectangle $(( TITLE_CARD_LEFT - 4 )),${TITLE_CARD_TOP} $(( TITLE_CARD_LEFT + TITLE_CARD_W + 12 )),$(( TITLE_CARD_TOP + TITLE_CARD_H )) rectangle 0,300 ${WIN_W},${WIN_H}" \
    -trim -format '%X,%Y,%w,%h' info: 2>/dev/null || true)
if [[ "$TOOLTIP_BOUNDS" =~ ^\+([0-9]+),\+([0-9]+),([0-9]+),([0-9]+)$ ]]; then
    TOOLTIP_X=${BASH_REMATCH[1]}
    TOOLTIP_Y=${BASH_REMATCH[2]}
    TOOLTIP_W=${BASH_REMATCH[3]}
    TOOLTIP_H=${BASH_REMATCH[4]}
else
    fail "normal-card hover did not reveal the full-title tooltip (${TOOLTIP_BOUNDS:-empty})"
fi
[ "$TOOLTIP_W" -ge 300 ] \
    && [ "$TOOLTIP_W" -le 500 ] \
    && [ "$TOOLTIP_H" -ge 30 ] \
    && [ "$TOOLTIP_H" -le 100 ] \
    && [ "$(( TOOLTIP_X + TOOLTIP_W ))" -le "$WIN_W" ] \
    && [ "$(( TOOLTIP_X + TOOLTIP_W ))" -le "$(( TITLE_CARD_LEFT + 90 ))" ] \
    && [ "$(( TOOLTIP_Y + TOOLTIP_H ))" -le "$WIN_H" ] \
    || fail "full-title tooltip ${TOOLTIP_W}x${TOOLTIP_H}+${TOOLTIP_X}+${TOOLTIP_Y} is not wrapped, bounded, and viewport-safe"
TOOLTIP_BG=$(convert /output/beads-board-title-tooltip.png \
    -format "%[pixel:p{$(( TOOLTIP_X + 3 )),$(( TOOLTIP_Y + 3 ))}]" info:)
[ "$TOOLTIP_BG" = "$BOARD_GROUND" ] \
    || fail "full-title tooltip background $TOOLTIP_BG is not the opaque theme ground $BOARD_GROUND"

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
[ "${CARD_DIFF:-999999}" -le 20000 ] \
    || fail "the board closed under the pointer on a card (${CARD_DIFF}px changed)"

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
CARD_W=$(( LANE_W - 20 ))
CARD_H=46
SOURCE_LEFT=$(( 16 + LANE_W ))
SOURCE_TOP=70
PRESS_X=$(( SOURCE_LEFT + 70 ))
PRESS_Y=$(( SOURCE_TOP + 14 ))
PRESS_OFFSET_Y=$(( PRESS_Y - SOURCE_TOP ))
TITLE_TOP=$(( SOURCE_TOP + 6 ))
TITLE_BOTTOM=$(( TITLE_TOP + 17 ))
META_TOP=$(( TITLE_BOTTOM + 2 ))
[ "$PRESS_X" -gt "$SOURCE_LEFT" ] \
    && [ "$PRESS_X" -lt "$(( SOURCE_LEFT + CARD_W ))" ] \
    && [ "$PRESS_Y" -ge "$TITLE_TOP" ] \
    && [ "$PRESS_Y" -lt "$TITLE_BOTTOM" ] \
    && [ "$PRESS_Y" -lt "$META_TOP" ] \
    || fail "drag press ($PRESS_X,$PRESS_Y) is outside title bounds or inside metadata"
DONE_LEFT=$(( 16 + 4 * LANE_W ))
xdotool mousemove --sync --window "$WID" "$PRESS_X" "$PRESS_Y"
sleep 0.2
import -window "$WID" /output/beads-board-drag-base.png
xdotool mousedown 1
ARM_X=$(( PRESS_X + 3 ))
ARM_OFFSET_X=$(( ARM_X - SOURCE_LEFT ))
xdotool mousemove --sync --window "$WID" "$ARM_X" "$PRESS_Y"
sleep 0.1

# Same card geometry over the Done card: a substantial changed area proves the
# source card is painted above the existing lane content, not clipped into it.
OVER_X=$(( DONE_LEFT + ARM_OFFSET_X ))
OVER_Y=$(( SOURCE_TOP + PRESS_OFFSET_Y ))
xdotool mousemove --sync --window "$WID" "$OVER_X" "$OVER_Y"
sleep 0.25
import -window "$WID" /output/beads-board-drag-over-cards.png
OVER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-over-cards.png -compose difference -composite \
    -crop "${CARD_W}x${CARD_H}+${DONE_LEFT}+${SOURCE_TOP}" +repage \
    -colorspace Gray -threshold 5% -format "%[fx:mean*w*h]" info:)
awk -v changed="$OVER_CHANGED" 'BEGIN { exit !(changed >= 200) }' \
    || fail "drag ghost did not paint above the Done card (${OVER_CHANGED}px changed)"

# An accepting target gets a compact semantic border, not a lane-sized tint.
DONE_BORDER_X=$(( 8 + (4 * (WIN_W - 16) + 2) / 5 ))
DONE_BORDER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-over-cards.png -compose difference -composite \
    -crop "2x100+${DONE_BORDER_X}+120" +repage -colorspace Gray -threshold 5% \
    -format "%[fx:mean*w*h]" info:)
awk -v changed="$DONE_BORDER_CHANGED" 'BEGIN { exit !(changed >= 80) }' \
    || fail "Done target border changed only ${DONE_BORDER_CHANGED}px"
assert_drag_frame /output/beads-board-drag-over-cards.png \
    "$OVER_X" "$OVER_Y" over-card "$DONE_BORDER_X"

# Backlog is a rejected target. It gets a compact neutral border while the
# bare lower lane remains the same board ground.
BACKLOG_X=$(( ARM_OFFSET_X + 20 ))
xdotool mousemove --sync --window "$WID" "$BACKLOG_X" "$PRESS_Y"
sleep 0.25
import -window "$WID" /output/beads-board-drag-no-drop.png
BACKLOG_BORDER_X=8
NO_DROP_BORDER_CHANGED=$(convert /output/beads-board-drag-base.png \
    /output/beads-board-drag-no-drop.png -compose difference -composite \
    -crop "2x100+${BACKLOG_BORDER_X}+120" +repage -colorspace Gray -threshold 5% \
    -format "%[fx:mean*w*h]" info:)
awk -v changed="$NO_DROP_BORDER_CHANGED" 'BEGIN { exit !(changed >= 80) }' \
    || fail "Backlog no-drop border changed only ${NO_DROP_BORDER_CHANGED}px"
assert_drag_frame /output/beads-board-drag-no-drop.png \
    "$BACKLOG_X" "$PRESS_Y" no-drop "$BACKLOG_BORDER_X"
convert /output/beads-board-drag-base.png \
    -crop "${LANE_W}x8+8+220" +repage /tmp/beads-ground-before.png
convert /output/beads-board-drag-no-drop.png \
    -crop "${LANE_W}x8+8+220" +repage /tmp/beads-ground-no-drop.png
NO_DROP_GROUND_CHANGED=$(compare -metric AE /tmp/beads-ground-before.png \
    /tmp/beads-ground-no-drop.png null: 2>&1 || true)
NO_DROP_GROUND_CHANGED=${NO_DROP_GROUND_CHANGED%%.*}
[ "${NO_DROP_GROUND_CHANGED:-999}" -le 20 ] \
    || fail "Backlog no-drop tinted ${NO_DROP_GROUND_CHANGED}px of neutral ground"

# Stay outside the strip beyond its 150ms hover grace. The board must remain
# open for the gesture, the terminal geometry must stay fixed, and the ghost's
# opaque bounds must land within 3px of pointer minus the source press offset.
OUT_X=$(( WIN_W / 2 ))
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
BOARD_BODY_CROP="${WIN_W}x$(( WIN_H - 34 - 24 ))+0+34"
convert /output/beads-board-hover.png -crop "$BOARD_BODY_CROP" +repage \
    /tmp/beads-board-hover-body.png
xdotool mousemove --sync --window "$WID" "$(( WIN_W - 32 ))" 49
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
xdotool mousemove --sync --window "$WID" "$(( WIN_W - 14 ))" 49
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
[ "${RESTORED_DIFF:-9999}" -le 400 ] \
    || fail "the smaller-text button did not undo the larger one (${RESTORED_DIFF}px left over)"

# Pin and resize while there is still one region. Later split fixtures must not
# change the geometry whose edge and row deltas this phase measures.
TOP_ROWS=$(latest_rows "$WORKSPACE")
xdotool click 1
for _ in $(seq 1 20); do
    PIN_ROWS=$(latest_rows "$WORKSPACE")
    [ "$PIN_ROWS" -lt "$TOP_ROWS" ] && break
    sleep 0.2
done
[ "${PIN_ROWS:-$TOP_ROWS}" -lt "$TOP_ROWS" ] \
    || fail "pin click did not reserve terminal rows ($TOP_ROWS -> ${PIN_ROWS:-$TOP_ROWS})"
xdotool mousemove --sync --window "$WID" "$(( WIN_W - 20 ))" "$(( WIN_H - 20 ))"
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
        -format "%[pixel:p{$(( 8 + lane * LANE_W + 4 )),$(( EDGE - 8 ))}]" info:)
    [ "$PINNED_LANE_GROUND" = "$BOARD_GROUND" ] \
        || fail "pinned lane $lane ground $PINNED_LANE_GROUND is tinted instead of neutral $BOARD_GROUND"
done
xdotool mousemove --sync --window "$WID" 40 "$EDGE"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" 40 "$(( EDGE + 60 ))"
sleep 0.4
xdotool mouseup 1
for _ in $(seq 1 20); do
    GROWN_ROWS=$(latest_rows)
    [ "$GROWN_ROWS" -lt "$PIN_ROWS" ] && break
    sleep 0.2
done
[ "${GROWN_ROWS:-$PIN_ROWS}" -lt "$PIN_ROWS" ] \
    || fail "dragging the bottom bar down reserved no rows (still $PIN_ROWS)"
import -window "$(window_id)" /output/beads-board-resized.png
GROWN_EDGE=$(board_bottom /output/beads-board-resized.png)
[ "$(( GROWN_EDGE - EDGE ))" -ge 50 ] \
    || fail "the board painted $(( GROWN_EDGE - EDGE ))px taller, not the 60px dragged"
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
[ "${SHRUNK_ROWS:-0}" -eq "$PIN_ROWS" ] \
    || fail "dragging the bar back left $SHRUNK_ROWS rows instead of $PIN_ROWS"

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
[ "$(server_reported_not_detected "$WORKSPACE" "$SPLIT_RECORD_MARK")" = "1" ] \
    || fail "the real server never answered NotDetected for the split source"
SPLIT_BASE=$(latest_rows "$PINNED_WS")
SPLIT_OTHER=$(latest_rows "$OTHER_WS")
[ "$SPLIT_BASE" -gt 0 ] && [ "$SPLIT_OTHER" -gt 0 ] \
    || fail "the workspace split did not settle both region geometries"

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
[ "${SPLIT_PINNED:-$SPLIT_BASE}" -lt "$SPLIT_BASE" ] \
    || fail "the split region did not reserve rows when pinned"
xdotool mousemove --sync --window "$WID" "$(( 3 * WIDTH / 4 ))" "$(( HEIGHT / 2 ))"
xdotool click 1
sleep 0.3
SPLIT_PINNED=$(latest_rows "$PINNED_WS")
SPLIT_OTHER=$(latest_rows "$OTHER_WS")
import -window "$WID" /output/beads-board-split.png
[ "$SPLIT_PINNED" -lt "$SPLIT_OTHER" ] \
    || fail "board reserved rows outside its region (pinned $SPLIT_PINNED, other $SPLIT_OTHER)"

# Regions are independent, not exclusive: give the second region a board of its
# own and pin it too. Both must then be open, each holding its own rows.
SECOND=$(other_workspace "$WORKSPACE") || fail "the split produced no second workspace"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$SECOND\",\"name\":\"beads\",\"accent_color\":\"#22d3ee\",\"split_direction\":null,\"project_root\":null}"
inject "$(sample_board "$SECOND")"
WID=$(window_id)
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove --sync --window "$WID" "$(( WIDTH / 2 + TOP_BADGE_ICON_X ))" "$TOP_BADGE_ICON_Y"
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
    -crop "9x1+$(( WIDTH / 2 - 4 ))+${BOARD_Y}" +repage -format %k info:)
[ "${DIVIDER_BAND:-1}" -gt 1 ] \
    || fail "no region divider between two open boards at y=${BOARD_Y}"
BOTH_PINNED=$(latest_rows "$PINNED_WS")
[ "${BOTH_OTHER:-$SPLIT_OTHER}" -lt "$SPLIT_OTHER" ] \
    || fail "pinning the second region's board reserved nothing (still $SPLIT_OTHER rows)"
[ "$BOTH_PINNED" -eq "$SPLIT_PINNED" ] \
    || fail "the second board disturbed the first region ($SPLIT_PINNED -> $BOTH_PINNED rows)"

# A queue that has run dry says so where its first card would have been, rather
# than leaving its head floating over a void. Sampled in that slot: bare ground
# is one colour, and the ghost's dashed outline and its word are not.
inject "$(empty_board "$WORKSPACE")"
import -window "$(window_id)" /output/beads-board-empty.png
# The first card's slot in the left region's first lane, taken across the middle
# third of the lane. One colour means the empty-state ghost is missing.
LANE_W=$(( (WIDTH / 2 - 16) / 5 ))
EMPTY_SLOT=$(convert /output/beads-board-empty.png \
    -crop "$(( LANE_W / 3 ))x20+$(( 8 + LANE_W / 3 ))+86" +repage -format %k info:)
[ "${EMPTY_SLOT:-1}" -gt 1 ] \
    || fail "an empty queue left its lane blank instead of saying so"

# A workspace usually gains its root from CWD naming, which lands after the
# SessionList that seeds the eager requests. Both gaining and losing that root
# must ask again, and either answer may retire only this workspace's board.
ROOTED_REQUESTS=$(board_request_count "$WORKSPACE")
inject "{\"type\":\"WorkspaceNamed\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"project_root\":\"/work/scribe\"}"
[ "$(board_request_count "$WORKSPACE")" -gt "$ROOTED_REQUESTS" ] \
    || fail "naming a rooted workspace did not request its board"

inject "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$WORKSPACE\",\"protocol_version\":1,\"state\":\"NotDetected\"}"
for _ in $(seq 1 20); do
    ROOTED_NOT_DETECTED_ROWS=$(latest_rows "$PINNED_WS")
    [ "$ROOTED_NOT_DETECTED_ROWS" -gt "$BOTH_PINNED" ] && break
    sleep 0.2
done
[ "${ROOTED_NOT_DETECTED_ROWS:-$BOTH_PINNED}" -gt "$BOTH_PINNED" ] \
    || fail "NotDetected left the newly rooted workspace's pinned board open"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] \
    || fail "rooted NotDetected disturbed the neighbouring workspace's board"
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
[ "${REPINNED_ROWS:-$ROOTED_NOT_DETECTED_ROWS}" -lt "$ROOTED_NOT_DETECTED_ROWS" ] \
    || fail "restored board did not repin before the rootless transition"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] \
    || fail "repinning the restored board disturbed its neighbour"

ROOTLESS_REQUESTS=$(board_request_count "$WORKSPACE")
inject "{\"type\":\"WorkspaceNamed\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"\",\"project_root\":null}"
[ "$(board_request_count "$WORKSPACE")" -gt "$ROOTLESS_REQUESTS" ] \
    || fail "clearing a workspace root did not request its board"

inject "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$WORKSPACE\",\"protocol_version\":1,\"state\":\"NotDetected\"}"
for _ in $(seq 1 20); do
    ROOTLESS_NOT_DETECTED_ROWS=$(latest_rows "$PINNED_WS")
    [ "$ROOTLESS_NOT_DETECTED_ROWS" -gt "$REPINNED_ROWS" ] && break
    sleep 0.2
done
[ "${ROOTLESS_NOT_DETECTED_ROWS:-$REPINNED_ROWS}" -gt "$REPINNED_ROWS" ] \
    || fail "NotDetected left the newly rootless workspace's pinned board open"
[ "$(latest_rows "$OTHER_WS")" -eq "$BOTH_OTHER" ] \
    || fail "rootless NotDetected disturbed the neighbouring workspace's board"
import -window "$(window_id)" /output/beads-board-rootless-not-detected.png

# The lower-region badge is a separate render path from the titlebar badge.
# Exercise it last so its stacked topology cannot disturb the single-region
# resize or the side-by-side reservation proofs above.
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":\"/work/scribe\"}"
inject "$(sample_board "$WORKSPACE")"
WID=$(window_id)
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove --sync --window "$WID" "$(( WIDTH / 4 ))" "$(( HEIGHT / 2 ))"
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

GRID_HEIGHT=$(( HEIGHT - 34 - 24 ))
LOWER_BADGE_ICON_X=13
LOWER_BADGE_ICON_Y=$(( 34 + GRID_HEIGHT / 2 + 17 ))
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
[ "$(latest_rows "$LOWER_WS")" -eq "$LOWER_ROWS" ] \
    || fail "lower workspace label changed its Beads reservation"
import -window "$WID" /output/beads-board-lower-before-hover.png
xdotool mousemove --sync --window "$WID" "$LOWER_BADGE_ICON_X" "$LOWER_BADGE_ICON_Y"
sleep 0.5
import -window "$WID" /output/beads-board-lower-hover.png
LOWER_HOVER_DIFF=$(compare -metric AE /output/beads-board-lower-before-hover.png \
    /output/beads-board-lower-hover.png null: 2>&1 || true)
LOWER_HOVER_DIFF=${LOWER_HOVER_DIFF%%.*}
[ "${LOWER_HOVER_DIFF:-0}" -ge 10000 ] \
    || fail "lower Beads icon hover changed only ${LOWER_HOVER_DIFF}px"
xdotool click 1
for _ in $(seq 1 20); do
    LOWER_PINNED_ROWS=$(latest_rows "$LOWER_WS")
    [ "$LOWER_PINNED_ROWS" -lt "$LOWER_ROWS" ] && break
    sleep 0.2
done
[ "${LOWER_PINNED_ROWS:-$LOWER_ROWS}" -lt "$LOWER_ROWS" ] \
    || fail "lower Beads icon click did not pin its board"

echo "PASS: Beads Constellation rendered for $WORKSPACE; pin rows $BASE_ROWS -> $PIN_ROWS;" \
    "bar drag $EDGE -> $GROWN_EDGE reserved $PIN_ROWS -> $GROWN_ROWS rows;" \
    "split pinned=$SPLIT_PINNED other=$SPLIT_OTHER; both pinned=$BOTH_PINNED other=$BOTH_OTHER;" \
    "rooted NotDetected rows=$ROOTED_NOT_DETECTED_ROWS;" \
    "rootless NotDetected rows=$ROOTLESS_NOT_DETECTED_ROWS;" \
    "lower badge rows $LOWER_ROWS -> $LOWER_PINNED_ROWS"
