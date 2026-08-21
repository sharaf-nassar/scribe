#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

ROOT=/tmp/scribe-beads-root
PROJECT="$ROOT/real-board"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
export BD_NO_DAEMON=1

seed_fixture() {
    mkdir -p "$PROJECT"
    git -C "$PROJECT" init --quiet
    git -C "$PROJECT" config user.email e2e@example.invalid
    git -C "$PROJECT" config user.name 'Scribe E2E'
    (
        cd "$PROJECT"
        bd init --quiet --stealth --prefix e2e
        bd create 'Card detail epic' --id e2e-epic --type epic --priority 2 >/dev/null
        bd create 'Open detail blocker' --id e2e-blocker --type task --priority 0 \
            --description 'Blocks the detail fixture until its contract is met.' >/dev/null
        bd create 'Complete card detail' --id e2e-detail --type task --priority 1 \
            --description 'Description fixture for the detail panel.' \
            --acceptance 'Acceptance fixture survives bd show.' \
            --notes 'Notes fixture keeps implementation context.' \
            --design 'Design fixture documents the direct data flow.' \
            --spec-id '024-beads-card-detail' \
            --labels 'beads,detail-fixture' \
            --assignee 'fixture-owner' \
            --estimate 90 \
            --external-ref 'fixture://beads-card-detail' \
            --due '2030-02-01' >/dev/null
        bd update e2e-detail --parent e2e-epic >/dev/null
        bd dep add e2e-detail e2e-blocker >/dev/null
        bd comments add e2e-detail 'Oldest deterministic fixture comment.' \
            --author 'Fixture Author' >/dev/null
        bd comments add e2e-detail 'Newest deterministic fixture comment.' \
            --author 'Fixture Reviewer' >/dev/null
        bd create 'Detail dependent' --id e2e-dependent --type task --priority 2 >/dev/null
        bd dep add e2e-dependent e2e-detail >/dev/null
        bd create 'Closed detail fixture' --id e2e-closed --type task --priority 3 >/dev/null
        bd close e2e-closed --reason 'Closed fixture reason.' >/dev/null
        bd create 'Deferred detail fixture' --id e2e-deferred --type task --priority 2 \
            --defer '2030-03-01' >/dev/null
        bd create 'Real board refresh' --id e2e-ready --type task --priority 1 >/dev/null
        bd create 'Older ready card' --id e2e-order-old --type task --priority 0 >/dev/null
        sleep 1
        bd create 'Newer ready card' --id e2e-order-new --type task --priority 4 >/dev/null
        bd create 'Native close and undo' --id e2e-close --type task --priority 2 >/dev/null
        bd create 'Classifier wins after drop' --id e2e-classifier --type task --priority 3 \
            --defer '2030-03-02' >/dev/null
    )
}

if [ "${1:-}" = "--seed" ]; then
    seed_fixture
    exit 0
fi

if [ "${SCRIBE_BEADS_PRESEEDED:-0}" != "1" ]; then
    seed_fixture
    scribe-test daemon stop
    scribe-test server stop
    printf '[workspaces]\nroots = ["%s"]\n' "$ROOT" >"$HOME/.config/scribe/config.toml"
    scribe-test server start
    scribe-test daemon start
    SESSION=$(scribe-test session create --cwd "$PROJECT")
    scribe-test wait-cwd "$SESSION" "$PROJECT"
fi

if [ "${SCRIBE_BEADS_PRESEEDED:-0}" = "1" ]; then
    BOARD=
    for _ in $(seq 1 50); do
        BOARD=$(python3 - "$RECORD" <<'PY'
import json, sys

found = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    state = message.get("state", {})
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsBoard"
            and isinstance(state, dict)
            and "Ready" in state):
        found = state
if found is not None:
    print(json.dumps(found, separators=(",", ":")))
PY
)
        [ -n "$BOARD" ] && break
        sleep 0.2
    done
    [ -n "$BOARD" ] || fail "client wire recorded no ready Beads board"
else
    BOARD=$(scribe-test beads-board)
fi

printf '%s\n' "$BOARD" | grep -q '"Ready"' \
    || fail "board refresh did not reach Ready: $BOARD"
READY_LANE=${BOARD#*'"ready":['}
READY_LANE=${READY_LANE%%'],"in_progress"'*}
printf '%s\n' "$READY_LANE" | grep -Fq '"id":"e2e-ready","title":"Real board refresh"' \
    || fail "seeded ready issue was absent from the Ready lane: $BOARD"
printf '%s\n' "$BOARD" | grep -Fq '"ready_total":5' \
    || fail "Ready total included the epic record: $BOARD"
python3 - "$BOARD" <<'PY'
import json
import sys

state = json.loads(sys.argv[1])
ready = state["Ready"]["snapshot"]["ready"]
positions = {card["id"]: index for index, card in enumerate(ready)}
for issue_id in ("e2e-order-old", "e2e-order-new"):
    if issue_id not in positions:
        raise SystemExit(f"FAIL: {issue_id} was absent from the Ready lane")
if positions["e2e-order-new"] >= positions["e2e-order-old"]:
    raise SystemExit(
        "FAIL: newer Ready card appeared after older Ready card: "
        f"new={positions['e2e-order-new']} old={positions['e2e-order-old']}"
    )
PY
(
    cd "$PROJECT"
    bd delete e2e-order-old e2e-order-new --force >/dev/null
)
if printf '%s\n' "$BOARD" | grep -Fq '"id":"e2e-epic"'; then
    fail "epic appeared as a standalone board card: $BOARD"
fi
printf '%s\n' "$BOARD" | grep -Fq '"id":"e2e-detail","title":"Complete card detail","priority":1,"blocker_ids":["e2e-blocker"],"parent_epic_name":"Card detail epic"' \
    || fail "child card lost its parent epic name: $BOARD"

DETAIL=$(cd "$PROJECT" && bd show e2e-detail --json --include-comments --include-dependents)
CLOSED=$(cd "$PROJECT" && bd show e2e-closed --json --include-comments --include-dependents)
DEFERRED=$(cd "$PROJECT" && bd show e2e-deferred --json --include-comments --include-dependents)
printf '%s\n' "$DETAIL" >/output/beads-real-bd-show.json

for expected in \
    '"title": "Complete card detail"' \
    '"description": "Description fixture for the detail panel."' \
    '"acceptance_criteria": "Acceptance fixture survives bd show."' \
    '"notes": "Notes fixture keeps implementation context."' \
    '"design": "Design fixture documents the direct data flow."' \
    '"spec_id": "024-beads-card-detail"' \
    '"status": "open"' \
    '"priority": 1' \
    '"issue_type": "task"' \
    '"assignee": "fixture-owner"' \
    '"owner": "e2e@example.invalid"' \
    '"created_by": "Scribe E2E"' \
    '"due_at": "2030-02-01T00:00:00Z"' \
    '"estimated_minutes": 90' \
    '"external_ref": "fixture://beads-card-detail"' \
    '"parent": "e2e-epic"' \
    '"title": "Card detail epic"' \
    '"dependency_type": "parent-child"' \
    '"id": "e2e-blocker"' \
    '"id": "e2e-dependent"' \
    '"author": "Fixture Author"' \
    '"text": "Oldest deterministic fixture comment."' \
    '"author": "Fixture Reviewer"' \
    '"text": "Newest deterministic fixture comment."'
do
    printf '%s\n' "$DETAIL" | grep -Fq "$expected" \
        || fail "seeded detail omitted $expected: $DETAIL"
done
printf '%s\n' "$DETAIL" | grep -Fq '"beads"' \
    || fail "seeded detail omitted beads label: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Fq '"detail-fixture"' \
    || fail "seeded detail omitted detail-fixture label: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Eq '"created_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "seeded detail omitted created_at: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Eq '"updated_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "seeded detail omitted updated_at: $DETAIL"
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "closed fixture stayed open: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Fq '"close_reason": "Closed fixture reason."' \
    || fail "closed fixture omitted its reason: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Eq '"closed_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "closed fixture omitted closed_at: $CLOSED"
printf '%s\n' "$DEFERRED" | grep -Fq '"defer_until": "2030-03-01T00:00:00Z"' \
    || fail "deferred fixture omitted defer_until: $DEFERRED"

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Flow epic admission]]
# The Flow graph is a server-owned decision over the same real `bd` read, so it
# is asserted here through `scribe-test` rather than through pixels. This runs
# on the daemon's own window, so it adds no region to the GPUI client and the
# later pointer phases keep the single-region geometry they were written for.
# The fixture derives its project path from this root when it is sourced, so it
# has to be exported first. Seeding it under $ROOT keeps it inside the same
# configured workspace root as the board fixture without sharing its tracker.
export BEADS_FLOW_ROOT="$ROOT"
. /tests/fixtures/beads-flow-epic.sh
seed_beads_flow_epic_fixture
verify_beads_flow_epic_fixture

graph_outcome() {
    scribe-test beads-epic-graph "$1"
}

# Same epic id, a region that never seeded it: the server resolves the graph
# from the requesting workspace's own root, so this must not leak across.
ISOLATED=$(graph_outcome flow-epic)
printf '%s\n' "$ISOLATED" | grep -Fq '"no_graph"' \
    || fail "flow-epic leaked into the real-board region: $ISOLATED"
printf '%s\n' "$ISOLATED" | grep -Fq '"no_epic"' \
    || fail "cross-region epic miss was not reported as no_epic: $ISOLATED"

# The create is what binds the new workspace to the Flow project root, and a
# non-Loading board is the real precondition for a graph: assembly reads the
# cached list rather than issuing a second `bd` query.
scribe-test session create --cwd "$BEADS_FLOW_PROJECT" >/dev/null
FLOW_BOARD=$(scribe-test beads-board)
printf '%s\n' "$FLOW_BOARD" | grep -Fq '"id":"flow-release"' \
    || fail "the Flow workspace board never carried its own issues: $FLOW_BOARD"

ADMITTED=$(graph_outcome flow-epic)
printf '%s\n' "$ADMITTED" >/output/beads-flow-graph.json
python3 - "$ADMITTED" <<'PY'
import json
import sys

outcome = json.loads(sys.argv[1])
graph = outcome.get("graph")
if graph is None:
    raise SystemExit(f"FAIL: flow-epic was not admitted: {outcome}")
nodes = {node["id"]: node for node in graph["nodes"]}
edges = {(edge["from"], edge["to"]) for edge in graph["edges"]}
if graph["total"] != 7 or len(nodes) != 7:
    raise SystemExit(f"FAIL: expected seven members, got {graph['total']}/{len(nodes)}")
if graph["closed"] != 1:
    raise SystemExit(f"FAIL: expected one closed member, got {graph['closed']}")
# The satisfied edge is the load-bearing one: its blocker is closed, so
# `bd blocked` cannot report it, yet the graph must still draw it.
if ("flow-foundation", "flow-api") not in edges:
    raise SystemExit(f"FAIL: satisfied closed-blocker edge missing: {sorted(edges)}")
if nodes["flow-foundation"]["status"] != "closed":
    raise SystemExit("FAIL: the satisfied blocker was not carried as closed")
fan_out = {to for (frm, to) in edges if frm == "flow-foundation"}
fan_in = {frm for (frm, to) in edges if to == "flow-integration"}
if fan_out != {"flow-api", "flow-ui", "flow-data"}:
    raise SystemExit(f"FAIL: fan-out did not survive assembly: {fan_out}")
if fan_in != {"flow-api", "flow-ui", "flow-data"}:
    raise SystemExit(f"FAIL: fan-in did not survive assembly: {fan_in}")
if len(edges) != 8:
    raise SystemExit(f"FAIL: expected eight edges, got {len(edges)}")
PY

# Both tracker-representable refusals leave the board in lanes. bd cannot store
# a cycle, so that admission arm stays a unit case over an in-memory graph.
REFUSED=$(graph_outcome flow-inadmissible-epic)
printf '%s\n' "$REFUSED" | grep -Fq '"no_graph"' \
    || fail "inadmissible epic was not refused: $REFUSED"
printf '%s\n' "$REFUSED" | grep -Eq '"(disconnected|external_blocker)"' \
    || fail "inadmissible epic gave no admission reason: $REFUSED"

NOT_AN_EPIC=$(graph_outcome flow-external-blocker)
printf '%s\n' "$NOT_AN_EPIC" | grep -Fq '"no_epic"' \
    || fail "a non-epic issue returned something other than no_epic: $NOT_AN_EPIC"

echo 'PASS: real bd admitted the Flow epic with its satisfied blocker edge, refused both' \
    'inadmissible shapes, and kept epic ids inside their own region'

if [ -z "${DISPLAY:-}" ]; then
    echo 'PASS: real bd refreshed the board and returned complete deterministic detail fixtures'
    exit 0
fi

WID=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
[ -n "$WID" ] || fail "real-bd detail run found no Scribe window"
xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID"
eval "$(xdotool getwindowgeometry --shell "$WID")"
WIN_W=$WIDTH
import -window "$WID" /output/beads-real-before.png

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Painted A2 geometry]]
# Every coordinate below comes from exactly two sources: the generated machine
# contract for the constants the mock fixes, and the pixels the shipped client
# actually painted for where its adaptive rail put each track. The rail moves
# with occupancy, text scale, and whether a collapsible queue is a 36px tab, an
# open drawer, or a pinned lane, so no arithmetic over five equal lanes can name
# a card or a drop target on this board.
CONTRACT=${SCRIBE_A2A3_CONTRACT:-/mocks/a2a3-contract.json}
[ -r "$CONTRACT" ] \
    || fail "the generated A2/A3 contract manifest is not mounted at $CONTRACT"
eval "$(python3 - "$CONTRACT" <<'PY'
import json, sys

geometry = json.load(open(sys.argv[1]))["geometry"]
wanted = {
    "a2": ("strip_h", "lanes_padding_top", "lanes_padding_left", "lanes_padding_right",
           "track_gap", "tab_w", "head_h", "headband_h", "row_h", "body_rows", "floor_h",
           "drawer_top", "drawer_bottom", "drawer_right", "drawer_w",
           "zoom_left", "zoom_top", "zoom_glyph_w", "zoom_glyph_h", "zoom_gap"),
    "a3": ("band_h", "band_pad_left", "graph_top", "graph_h", "node_w", "node_h",
           "left_pad", "rank_pitch", "hbar_top", "hbar_h"),
}
for section, keys in wanted.items():
    for key in keys:
        print(f"{section.upper()}_{key.upper()}={int(geometry[section][key])}")
PY
)"

# The one number no contract owns: the OS titlebar above the strip. It is
# measured, never assumed -- `rail` reports the strip's own top from the row
# where every track paints its state seam.
STRIP_TOP=
RAIL_TRACKS=

rail_report() {
    import -window "$WID" "$1"
    python3 /tests/func/beads-board-geometry.py rail \
        --contract "$CONTRACT" --shot "$1" --width "$WIN_W"
}

# Re-measure before every gesture: a card that changed lanes reflows the rail
# behind it, so geometry resolved for the previous gesture is already stale.
measure_rail() {
    local report tracks
    report=$(rail_report "${1:-/output/beads-rail.png}") \
        || fail "the painted board exposed no A2 seam row to measure"
    STRIP_TOP=$(printf '%s\n' "$report" | head -1)
    RAIL_TRACKS=$(printf '%s\n' "$report" | tail -n +2)
    tracks=$(printf '%s\n' "$RAIL_TRACKS" | wc -l)
    [ "$tracks" -eq 5 ] || fail "the painted rail showed $tracks tracks, not five"
}

track_field() {
    printf '%s\n' "$RAIL_TRACKS" | sed -n "$(( $1 + 1 ))p" | cut -d' ' -f"$2"
}

# A pointer x inside one painted track, whatever width that track currently
# holds: the centre of a full lane, of a 36px rail tab, or of a pinned lane.
lane_x() {
    echo $(( $(track_field "$1" 1) + $(track_field "$1" 2) / 2 ))
}

# One whole painted row's centre. `row_h` and `headband_h` are contract
# constants; the strip's own top is measured.
row_y() {
    echo $(( STRIP_TOP + A2_HEADBAND_H + $1 * A2_ROW_H + A2_ROW_H / 2 ))
}

# A lane head's own line, and the `×` a pinned Blocked/Done lane ends it with.
head_y() {
    echo $(( STRIP_TOP + A2_LANES_PADDING_TOP + A2_HEAD_H / 2 ))
}

lane_unpin_x() {
    echo $(( $(track_field "$1" 1) + $(track_field "$1" 2) - 4 ))
}

# Pin one collapsible lane open by clicking its rail tab, so the rows it holds
# are painted at all, and prove the pin landed.
pin_lane() {
    measure_rail
    xdotool mousemove --sync --window "$WID" "$(lane_x "$1")" "$(row_y 1)"
    sleep 0.3
    xdotool click 1
    sleep 0.6
    measure_rail
    [ "$(track_field "$1" 2)" -gt "$A2_TAB_W" ] \
        || fail "clicking track $1's tab left it $(track_field "$1" 2)px wide"
    xdotool mousemove --sync --window "$WID" 13 17
    sleep 0.5
}

unpin_lane() {
    measure_rail
    xdotool mousemove --sync --window "$WID" "$(lane_unpin_x "$1")" "$(head_y)"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

strip_crop() {
    echo "${WIN_W}x${A2_STRIP_H}+0+${STRIP_TOP}"
}

# The centre of the collapsed drawer's own interior (A2-G8), for a hover that
# has to land on the drawer rather than on the tab that opened it.
drawer_center_x() {
    echo $(( WIN_W - A2_DRAWER_RIGHT - A2_DRAWER_W / 2 ))
}

crop_diff() {
    local diff
    diff=$(compare -metric AE \
        \( "$1" -crop "$3" +repage \) \( "$2" -crop "$3" +repage \) null: 2>&1 || true)
    printf '%s\n' "${diff%%.*}"
}

flow_detail_requests() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "RequestBeadsIssueDetail"
            and message.get("issue_id") == sys.argv[2]):
        count += 1
print(count)
PY
}

board_request_count() {
    python3 - "$RECORD" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if row.get("dir") == "client" and message.get("type") == "RequestBeadsBoard":
        count += 1
print(count)
PY
}

write_request_count() {
    python3 - "$RECORD" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "BeadsIssueWrite"
            and message.get("issue_id") == "e2e-detail"):
        count += 1
print(count)
PY
}

key_input_count() {
    python3 - "$RECORD" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    if row.get("dir") == "client" and row.get("message", {}).get("type") == "KeyInput":
        count += 1
print(count)
PY
}

write_failure_seen() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
needle = sys.argv[2].lower()
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsIssueWriteResult"
            and message.get("issue_id") == "e2e-detail"):
        result = json.dumps(message.get("result", {}), separators=(",", ":")).lower()
        if "failed" in result and needle in result:
            raise SystemExit(0)
raise SystemExit(1)
PY
}

wait_for_write_failure() {
    local needle="$1" attempts="$2"
    for _ in $(seq 1 "$attempts"); do
        write_failure_seen "$needle" && return 0
        sleep 0.2
    done
    return 1
}

detail_response_seen() {
    python3 - "$RECORD" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsIssueDetail"
            and message.get("issue_id") == "e2e-detail"
            and message.get("detail")):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

issue_position() {
    python3 - "$RECORD" "$1" "${2:-0}" <<'PY'
import json, sys

snapshot = None
for line_number, line in enumerate(open(sys.argv[1]), 1):
    if line_number <= int(sys.argv[3]):
        continue
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    state = message.get("state", {})
    if row.get("dir") == "server" and message.get("type") == "BeadsBoard":
        ready = state.get("Ready") if isinstance(state, dict) else None
        if isinstance(ready, dict):
            snapshot = ready.get("snapshot")
if not isinstance(snapshot, dict):
    raise SystemExit(1)
for lane, key in enumerate(("backlog", "ready", "in_progress", "blocked", "done")):
    for index, card in enumerate(snapshot.get(key, [])):
        if card.get("id") == sys.argv[2]:
            print(lane, index)
            raise SystemExit
raise SystemExit(1)
PY
}

wait_for_lane() {
    local issue="$1" expected="$2" position
    for _ in $(seq 1 50); do
        position=$(issue_position "$issue" 2>/dev/null || true)
        [ "${position%% *}" = "$expected" ] && return 0
        sleep 0.2
    done
    fail "$issue did not reach lane $expected (last ${position:-missing})"
}

wait_for_lane_after_result() {
    local issue="$1" expected="$2" result_line="$3" position
    for _ in $(seq 1 50); do
        position=$(issue_position "$issue" "$result_line" 2>/dev/null || true)
        [ "${position%% *}" = "$expected" ] && return 0
        sleep 0.2
    done
    fail "$issue did not reach lane $expected after write result (last ${position:-missing})"
}

issue_write_count() {
    python3 - "$RECORD" "$1" "${2:-}" <<'PY'
import json, sys

count = 0
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if (row.get("dir") != "client"
            or message.get("type") != "BeadsIssueWrite"
            or message.get("issue_id") != sys.argv[2]):
        continue
    verb = message.get("verb")
    name = verb if isinstance(verb, str) else next(iter(verb), "")
    if not sys.argv[3] or name == sys.argv[3]:
        count += 1
print(count)
PY
}

wait_for_write() {
    local issue="$1" verb="$2" before="$3"
    for _ in $(seq 1 50); do
        [ "$(issue_write_count "$issue" "$verb")" -eq "$(( before + 1 ))" ] && return 0
        sleep 0.2
    done
    fail "$issue sent no $verb write"
}

issue_applied_count() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys

count = 0
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    result = message.get("result")
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsIssueWriteResult"
            and message.get("issue_id") == sys.argv[2]
            and isinstance(result, dict)
            and "applied" in result):
        count += 1
print(count)
PY
}

wait_for_applied() {
    local issue="$1" before="$2"
    for _ in $(seq 1 50); do
        [ "$(issue_applied_count "$issue")" -eq "$(( before + 1 ))" ] && return 0
        sleep 0.2
    done
    fail "$issue returned no applied write result"
}

issue_write_result_after() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys

for line_number, line in enumerate(open(sys.argv[1]), 1):
    if line_number <= int(sys.argv[3]):
        continue
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsIssueWriteResult"
            and message.get("issue_id") == sys.argv[2]):
        print(line_number, json.dumps(message.get("result"), separators=(",", ":")))
        raise SystemExit
raise SystemExit(1)
PY
}

wait_for_applied_line() {
    local issue="$1" after_line="$2" result line payload
    for _ in $(seq 1 50); do
        result=$(issue_write_result_after "$issue" "$after_line" 2>/dev/null || true)
        [ -n "$result" ] && break
        sleep 0.2
    done
    [ -n "${result:-}" ] || fail "$issue returned no write result"
    line=${result%% *}
    payload=${result#* }
    printf '%s\n' "$payload" | grep -Fq '"applied"' \
        || fail "$issue returned non-Applied result: $payload"
    printf '%s\n' "$line"
}

mouse_report_count() {
    python3 - "$RECORD" <<'PY'
import json, sys

count = 0
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "KeyInput"):
        data = bytes(message.get("data") or [])
        count += data.startswith(b"\x1b[<") and data[-1:] in (b"M", b"m")
print(count)
PY
}

# SGR frames whose button is a wheel notch (64/65), as opposed to the motion
# and button frames `mouse_report_count` also counts: what a wheel over Flow
# must never send the pane is a wheel.
wheel_report_count() {
    python3 - "$RECORD" <<'PY'
import json, sys

count = 0
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    if row.get("dir") == "client" and message.get("type") == "KeyInput":
        data = bytes(message.get("data") or [])
        if data.startswith(b"\x1b[<") and data[-1:] in (b"M", b"m"):
            count += data[3:].split(b";")[0] in (b"64", b"65")
print(count)
PY
}

wait_for_mouse_report() {
    local before="$1"
    # DECSET crosses the shell, PTY, server Term, and client Term asynchronously.
    # A fixed sleep can send the probe before the client has parsed the modes;
    # the first actual SGR report is the readiness signal instead.
    for _ in $(seq 1 50); do
        xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 - 40 ))" 400
        xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 + 40 ))" 400
        [ "$(mouse_report_count)" -gt "$before" ] && return 0
        sleep 0.2
    done
    return 1
}

# Press one painted card and release it over one painted track. Both ends are
# resolved from the live rail, so a collapsed 36px tab, a pinned lane, and a
# full lane are all the same gesture with no target-specific arithmetic.
drag_issue() {
    local issue="$1" target_lane="$2" position source_lane index
    local press_x press_y target_x reports_before reports_after
    # A native release ends the hover overlay. Re-enter through the painted bead
    # target before resolving fresh card geometry for every gesture.
    xdotool mousemove --sync --window "$WID" 13 17
    sleep 0.5
    position=$(issue_position "$issue") || fail "$issue has no painted card"
    read -r source_lane index <<<"$position"
    [ "$index" -lt "$A2_BODY_ROWS" ] \
        || fail "$issue sits past the last whole painted row of its lane"
    measure_rail
    press_x=$(lane_x "$source_lane")
    press_y=$(row_y "$index")
    target_x=$(lane_x "$target_lane")
    xdotool mousemove --sync --window "$WID" "$press_x" "$press_y"
    sleep 0.2
    reports_before=$(mouse_report_count)
    xdotool mousedown 1
    xdotool mousemove --sync --window "$WID" "$(( press_x + 3 ))" "$press_y"
    xdotool mousemove --sync --window "$WID" "$target_x" "$press_y"
    sleep 0.2
    xdotool mouseup 1
    sleep 0.2
    reports_after=$(mouse_report_count)
    [ "$reports_after" -eq "$reports_before" ] \
        || fail "$issue drag leaked mouse reports ($reports_before -> $reports_after)"
    xdotool mousemove --sync --window "$WID" 13 17
    sleep 0.5
}

# Hover the rooted workspace's bead and wait for the real board to paint.
xdotool mousemove --sync --window "$WID" 13 17
for _ in $(seq 1 30); do
    sleep 0.2
    import -window "$WID" /output/beads-real-board.png
    BOARD_DIFF=$(compare -metric AE /output/beads-real-before.png \
        /output/beads-real-board.png null: 2>&1 || true)
    BOARD_DIFF=${BOARD_DIFF%%.*}
    [ "${BOARD_DIFF:-0}" -ge 10000 ] && break
done
[ "${BOARD_DIFF:-0}" -ge 10000 ] || fail "real bd board never painted"

# The press and release stay at one coordinate, inside the future two-pixel
# drag arm. Resolve the detail card from the authoritative lane order rather
# than assuming priority order keeps it first.
measure_rail
# The rail this board actually painted: three ledger lanes and two collapsed
# 36px tabs, the first lane starting at the contract's own left control gutter.
[ "$(track_field 0 1)" -eq "$A2_LANES_PADDING_LEFT" ] \
    || fail "the first lane started at $(track_field 0 1), not the ${A2_LANES_PADDING_LEFT}px gutter"
[ "$(track_field 3 2)" -eq "$A2_TAB_W" ] && [ "$(track_field 4 2)" -eq "$A2_TAB_W" ] \
    || fail "the collapsed rail is not two ${A2_TAB_W}px tabs: $RAIL_TRACKS"

DETAIL_POSITION=$(issue_position e2e-detail) || fail "e2e-detail has no painted card"
read -r DETAIL_LANE DETAIL_INDEX <<<"$DETAIL_POSITION"
# The detail fixture is blocked, and Blocked is a collapsed tab: pin that lane
# so the row exists to click, which is also A2-I4 through a pinned lane.
if [ "$DETAIL_LANE" -ge 3 ]; then
    pin_lane "$DETAIL_LANE"
    import -window "$WID" /output/beads-real-board.png
fi
[ "$DETAIL_INDEX" -lt "$A2_BODY_ROWS" ] \
    || fail "e2e-detail sits past the last whole painted row of its lane"
DETAIL_X=$(lane_x "$DETAIL_LANE")
DETAIL_Y=$(row_y "$DETAIL_INDEX")
REQUESTS_BEFORE=$(flow_detail_requests e2e-detail)
xdotool mousemove --sync --window "$WID" "$DETAIL_X" "$DETAIL_Y"
xdotool mousedown 1
xdotool mouseup 1
for _ in $(seq 1 30); do
    [ "$(flow_detail_requests e2e-detail)" -eq "$((REQUESTS_BEFORE + 1))" ] && break
    sleep 0.2
done
[ "$(flow_detail_requests e2e-detail)" -eq "$((REQUESTS_BEFORE + 1))" ] \
    || fail "sub-2px card click sent no detail request"
for _ in $(seq 1 50); do
    detail_response_seen && break
    sleep 0.2
done
detail_response_seen || fail "real server sent no matching detail response"

python3 /tests/func/assert-beads-detail-wire.py \
    --bd /output/beads-real-bd-show.json \
    --wire "$RECORD" \
    --issue e2e-detail \
    --output /output/beads-real-detail-evidence.json \
    || fail "painted detail response diverged from bd show"
# Everything below this point works on the panel, and the drag matrix expects
# the collapsed rail it measured above.
if [ "$DETAIL_LANE" -ge 3 ]; then
    unpin_lane "$DETAIL_LANE"
fi
xdotool mousemove --sync --window "$WID" 900 600
sleep 0.5
import -window "$WID" /output/beads-real-detail.png
DETAIL_DIFF=$(compare -metric AE /output/beads-real-board.png \
    /output/beads-real-detail.png null: 2>&1 || true)
DETAIL_DIFF=${DETAIL_DIFF%%.*}
[ "${DETAIL_DIFF:-0}" -ge 20000 ] || fail "real detail panel changed only ${DETAIL_DIFF:-0}px"

# Bound the panel by differencing the pre-click board against the post-click
# window, with the board strip masked out of that difference first.
#
# The mask is load-bearing. This probe once assumed the panel was the only
# thing a card click changed, which stopped being true when Flow landed: a
# click now opens the panel *and* swaps the strip into that card's epic graph,
# so an unmasked difference spans strip plus panel and its bounding box is no
# longer the panel's. It measured 561x553+223+47 — the strip's own left edge
# one column further out, and 188px of extra height reaching up into the
# strip — where the panel is still exactly the 560px surface it always was.
# The strip is the contract's own `strip_h` reservation directly under the
# measured strip top, which is where this crop already starts, so blanking the
# crop's first `strip_h` rows leaves only what the panel itself changed.
panel_bounds() {
    local before="$1" after="$2" content_height=$(( HEIGHT - STRIP_TOP - 24 ))
    convert \
        \( "$before" -crop "${WIN_W}x${content_height}+0+${STRIP_TOP}" \) \
        \( "$after" -crop "${WIN_W}x${content_height}+0+${STRIP_TOP}" \) \
        -compose difference -composite -threshold 10% \
        -fill black -draw "rectangle 0,0 $(( WIN_W - 1 )),$(( A2_STRIP_H - 1 ))" -trim \
        -format '%w %h %X %Y' info:
}

# The field targets follow the painted panel rather than a lane-specific or
# fixed-window offset. This keeps the real input proof valid when the panel
# centers inside a resized or split terminal region.
read -r PANEL_W PANEL_H PANEL_X PANEL_Y \
    <<<"$(panel_bounds /output/beads-real-board.png /output/beads-real-detail.png)"
[ "$PANEL_H" -ge 120 ] && { [ "$PANEL_W" -eq 560 ] || [ "$PANEL_W" -eq 590 ]; } \
    || fail "resolved detail panel bounds were ${PANEL_W}x${PANEL_H}${PANEL_X}${PANEL_Y}"
# The threshold may detect the exact 560px surface or include `shadow_lg`'s
# 15px on each side. Both share the surface's left/top edge, so normalize the
# width before deriving field coordinates.
PANEL_W=560
PANEL_LEFT=${PANEL_X#+}
PANEL_TOP=${PANEL_Y#+}
PANEL_ID_X=32
PANEL_ID_Y=60
PANEL_OUTSIDE_X=$(( PANEL_LEFT + PANEL_W + 16 ))
[ "$PANEL_OUTSIDE_X" -lt "$WIN_W" ] || PANEL_OUTSIDE_X=$(( PANEL_LEFT - 16 ))
PANEL_OUTSIDE_Y=$(( PANEL_TOP + PANEL_H + 16 ))
[ "$PANEL_OUTSIDE_Y" -lt "$HEIGHT" ] || PANEL_OUTSIDE_Y=$(( PANEL_TOP - 16 ))
panel_move() {
    xdotool mousemove --sync --window "$WID" "$(( PANEL_LEFT + $1 ))" "$(( PANEL_TOP + $2 ))"
}

panel_move_outside() {
    xdotool mousemove --sync --window "$WID" "$PANEL_OUTSIDE_X" "$PANEL_OUTSIDE_Y"
}

# The painted panel's identity target is the final semantic check: it copies
# the same full id read from bd show and carried on the matched wire response.
panel_move "$PANEL_ID_X" "$PANEL_ID_Y"
xdotool click 1
sleep 0.2
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "e2e-detail" ] || fail "painted panel copied '$COPIED' instead of e2e-detail"

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Real pointer editor checkpoint]]
# The real editor owns keyboard input while armed. The transparent wire tap is
# the byte-level pane oracle, and the MessagePack name proves the pointer path
# committed the title through the serialized SetTitle variant.
EDITOR_TITLE='Complete card detail pointer checked'
EDITOR_WRITES_BEFORE=$(issue_write_count e2e-detail set_title)
EDITOR_APPLIED_BEFORE=$(issue_applied_count e2e-detail)
KEYS_BEFORE=$(key_input_count)
panel_move 188 23
xdotool click 1
sleep 0.2
import -window "$WID" /output/beads-editor-caret-before.png
xdotool key --clearmodifiers Left
sleep 0.2
import -window "$WID" /output/beads-editor-caret-after.png
EDITOR_VISUAL_CROP="440x32+$(( PANEL_LEFT + 100 ))+$(( PANEL_TOP + 8 ))"
convert /output/beads-editor-caret-before.png -crop "$EDITOR_VISUAL_CROP" +repage \
    /output/beads-editor-caret-before-crop.png
convert /output/beads-editor-caret-after.png -crop "$EDITOR_VISUAL_CROP" +repage \
    /output/beads-editor-caret-after-crop.png
CARET_DIFF=$(compare -metric AE /output/beads-editor-caret-before-crop.png \
    /output/beads-editor-caret-after-crop.png null: 2>&1 || true)
CARET_DIFF=${CARET_DIFF%%.*}
[ "${CARET_DIFF:-0}" -ge 20 ] && [ "${CARET_DIFF:-0}" -le 500 ] \
    || fail "moving the editor caret changed ${CARET_DIFF:-0}px"
xdotool key --clearmodifiers ctrl+a
sleep 0.2
import -window "$WID" /output/beads-editor-selection.png
convert /output/beads-editor-selection.png -crop "$EDITOR_VISUAL_CROP" +repage \
    /output/beads-editor-selection-crop.png
SELECTION_DIFF=$(compare -metric AE /output/beads-editor-caret-after-crop.png \
    /output/beads-editor-selection-crop.png null: 2>&1 || true)
SELECTION_DIFF=${SELECTION_DIFF%%.*}
[ "${SELECTION_DIFF:-0}" -ge 200 ] \
    || fail "select-all painted only ${SELECTION_DIFF:-0}px"
xdotool type --clearmodifiers --delay 5 -- "$EDITOR_TITLE"
sleep 0.2
KEYS_AFTER=$(key_input_count)
[ "$KEYS_AFTER" -eq "$KEYS_BEFORE" ] \
    || fail "armed Beads editor forwarded keystrokes to the pane program"
[ "$(issue_write_count e2e-detail set_title)" -eq "$EDITOR_WRITES_BEFORE" ] \
    || fail "typing into the armed editor committed before Enter"
xdotool key --clearmodifiers Return
wait_for_write e2e-detail set_title "$EDITOR_WRITES_BEFORE"
wait_for_applied e2e-detail "$EDITOR_APPLIED_BEFORE"
EDITOR_SHOW=$(cd "$PROJECT" && bd show e2e-detail --json)
printf '%s\n' "$EDITOR_SHOW" | grep -Fq "\"title\": \"$EDITOR_TITLE\"" \
    || fail "pointer title edit did not persist exactly once: $EDITOR_SHOW"
printf '%s\n' \
    'PASS: real pointer editor painted caret and selection, then committed set_title without KeyInput leakage' \
    >/output/beads-editor-pointer-checkpoint.txt
echo 'CHECKPOINT: real pointer editor painted feedback and committed without KeyInput leakage'

# Install the deterministic bd fault shim after the capability handshake. It
# delegates every read and targets only writes for this fixture.
mv /usr/local/bin/bd /usr/local/bin/bd-real
ln -s /tests/fixtures/bd-write-fault.sh /usr/local/bin/bd
rm -f /tmp/scribe-beads-write-fault-mode

xdotool mousemove --sync --window "$WID" 900 600
sleep 0.2
import -window "$WID" /output/beads-write-last-good.png

printf '%s\n' nonzero:e2e-detail >/tmp/scribe-beads-write-fault-mode
panel_move 188 23
xdotool click 1
xdotool type --clearmodifiers --delay 5 -- 'must not persist'
xdotool key --clearmodifiers Return
wait_for_write_failure 'forced nonzero write' 50 \
    || fail "GPUI nonzero write produced no typed Failed result"
rm -f /tmp/scribe-beads-write-fault-mode
NONZERO_SHOW=$(cd "$PROJECT" && bd show e2e-detail --json)
printf '%s\n' "$NONZERO_SHOW" | grep -Fq "\"title\": \"$EDITOR_TITLE\"" \
    || fail "GPUI nonzero write replaced last-good detail: $NONZERO_SHOW"
panel_move_outside
sleep 0.2
import -window "$WID" /output/beads-write-nonzero-notice.png
NONZERO_NOTICE_DIFF=$(compare -metric AE /output/beads-write-last-good.png \
    /output/beads-write-nonzero-notice.png null: 2>&1 || true)
NONZERO_NOTICE_DIFF=${NONZERO_NOTICE_DIFF%%.*}
[ "${NONZERO_NOTICE_DIFF:-0}" -ge 500 ] \
    || fail "nonzero write painted no failure notice (${NONZERO_NOTICE_DIFF:-0}px)"

# Let the first one-line notice expire so the timeout proof starts from the
# same visible last-good detail. Timeout convergence must request both board
# and detail again while the persisted issue remains untouched.
sleep 5.2
TIMEOUT_DETAIL_BEFORE=$(flow_detail_requests e2e-detail)
TIMEOUT_BOARD_BEFORE=$(board_request_count)
printf '%s\n' timeout:e2e-detail >/tmp/scribe-beads-write-fault-mode
panel_move 188 23
xdotool click 1
xdotool type --clearmodifiers --delay 5 -- 'must not time in'
xdotool key --clearmodifiers Return
wait_for_write_failure 'bd issue write timed out' 100 \
    || fail "GPUI timeout write produced no typed Failed result"
rm -f /tmp/scribe-beads-write-fault-mode
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests e2e-detail)" -gt "$TIMEOUT_DETAIL_BEFORE" ] \
        && [ "$(board_request_count)" -gt "$TIMEOUT_BOARD_BEFORE" ] \
        && break
    sleep 0.2
done
[ "$(flow_detail_requests e2e-detail)" -gt "$TIMEOUT_DETAIL_BEFORE" ] \
    || fail "timeout did not request authoritative detail"
[ "$(board_request_count)" -gt "$TIMEOUT_BOARD_BEFORE" ] \
    || fail "timeout did not request an authoritative board"
TIMEOUT_SHOW=$(cd "$PROJECT" && bd show e2e-detail --json)
printf '%s\n' "$TIMEOUT_SHOW" | grep -Fq "\"title\": \"$EDITOR_TITLE\"" \
    || fail "GPUI timeout write replaced last-good detail: $TIMEOUT_SHOW"
printf '%s\n' "$TIMEOUT_SHOW" >/output/beads-write-gpui-final-show.json
panel_move_outside
sleep 0.2
import -window "$WID" /output/beads-write-timeout-notice.png
TIMEOUT_NOTICE_DIFF=$(compare -metric AE /output/beads-write-last-good.png \
    /output/beads-write-timeout-notice.png null: 2>&1 || true)
TIMEOUT_NOTICE_DIFF=${TIMEOUT_NOTICE_DIFF%%.*}
[ "${TIMEOUT_NOTICE_DIFF:-0}" -ge 500 ] \
    || fail "timeout write painted no failure notice (${TIMEOUT_NOTICE_DIFF:-0}px)"

echo 'PASS: real bd detail persisted, editor input stayed local, and write failures retained last-good state with notices'

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Card drag writes and pointer isolation]]
xdotool key --clearmodifiers Escape
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5

# Keep SingleController write authority: type the DEC modes through the client
# into its owner-visible pane, then use the passive wire tap as the oracle.
xdotool type --clearmodifiers --delay 1 "printf '\033[?1003h\033[?1006h'"
xdotool key Return
PROBE_BEFORE=$(mouse_report_count)
wait_for_mouse_report "$PROBE_BEFORE" \
    || fail "owner-visible pane did not enable SGR mouse reporting"
CLICK_BEFORE=$(mouse_report_count)
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" 400
xdotool click 1
for _ in $(seq 1 20); do
    [ "$(mouse_report_count)" -ge "$(( CLICK_BEFORE + 2 ))" ] && break
    sleep 0.2
done
[ "$(mouse_report_count)" -ge "$(( CLICK_BEFORE + 2 ))" ] \
    || fail "owner-visible pane click emitted no SGR mouse reports"
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5

CLAIM_BEFORE=$(issue_write_count e2e-ready claim)
CLAIM_APPLIED_BEFORE=$(issue_applied_count e2e-ready)
drag_issue e2e-ready 2
wait_for_write e2e-ready claim "$CLAIM_BEFORE"
wait_for_applied e2e-ready "$CLAIM_APPLIED_BEFORE"
wait_for_lane e2e-ready 2
CLAIMED=$(cd "$PROJECT" && bd show e2e-ready --json)
printf '%s\n' "$CLAIMED" | grep -Fq '"status": "in_progress"' \
    || fail "Ready drag did not claim through native bd: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Fq '"assignee": "Scribe E2E"' \
    || fail "native claim did not resolve the local actor: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Eq '"started_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "native claim omitted started_at: $CLAIMED"

CLOSE_BEFORE=$(issue_write_count e2e-close close_issue)
CLOSE_APPLIED_BEFORE=$(issue_applied_count e2e-close)
drag_issue e2e-close 4
wait_for_write e2e-close close_issue "$CLOSE_BEFORE"
wait_for_applied e2e-close "$CLOSE_APPLIED_BEFORE"
sleep 0.3
CLOSED_DROP=$(cd "$PROJECT" && bd show e2e-close --json)
printf '%s\n' "$CLOSED_DROP" | grep -Fq '"status": "closed"' \
    || fail "Done drop did not close through native bd: $CLOSED_DROP"
printf '%s\n' "$CLOSED_DROP" | grep -Eq '"closed_at": "[0-9]{4}-' \
    || fail "Done drop omitted native closed_at: $CLOSED_DROP"
import -window "$WID" /output/beads-drag-close-notice.png
UNDO_BEFORE=$(issue_write_count e2e-close undo_close)
UNDO_RECORD_BEFORE=$(wc -l <"$RECORD")
# The close notice shares the detail panel's centered geometry, so its Undo
# target follows the detected panel origin rather than the lane the card was
# dropped into.
panel_move 280 16
xdotool click 1
wait_for_write e2e-close undo_close "$UNDO_BEFORE"
UNDO_RESULT_LINE=$(wait_for_applied_line e2e-close "$UNDO_RECORD_BEFORE")
wait_for_lane_after_result e2e-close 1 "$UNDO_RESULT_LINE"
REOPENED_DROP=$(cd "$PROJECT" && bd show e2e-close --json)
printf '%s\n' "$REOPENED_DROP" | grep -Fq '"status": "open"' \
    || fail "board-side Undo did not restore open: $REOPENED_DROP"
if printf '%s\n' "$REOPENED_DROP" | grep -Eq '"closed_at": "'; then
    fail "board-side Undo retained closed_at: $REOPENED_DROP"
fi

DEFER_BEFORE=$(issue_write_count e2e-deferred set_status)
drag_issue e2e-deferred 1
wait_for_write e2e-deferred set_status "$DEFER_BEFORE"
wait_for_lane e2e-deferred 1
CLEARED=$(cd "$PROJECT" && bd show e2e-deferred --json)
printf '%s\n' "$CLEARED" | grep -Fq '"status": "open"' \
    || fail "Backlog drag did not restore open: $CLEARED"
if printf '%s\n' "$CLEARED" | grep -Eq '"defer_until": "'; then
    fail "Backlog drag retained defer_until: $CLEARED"
fi

# Make the already-painted Backlog card blocked behind the server cache. Its
# accepted Ready intent clears defer, then the authoritative classifier wins.
(cd "$PROJECT" && bd dep add e2e-classifier e2e-blocker >/dev/null)
import -window "$WID" /output/beads-drag-classifier-before.png
CLASSIFIER_BEFORE=$(issue_write_count e2e-classifier set_status)
drag_issue e2e-classifier 1
wait_for_write e2e-classifier set_status "$CLASSIFIER_BEFORE"
wait_for_lane e2e-classifier 3
sleep 0.3
CLASSIFIED=$(cd "$PROJECT" && bd show e2e-classifier --json)
printf '%s\n' "$CLASSIFIED" | grep -Fq '"status": "open"' \
    || fail "classifier fixture did not stay open: $CLASSIFIED"
import -window "$WID" /output/beads-drag-classifier-notice.png
NOTICE_CHANGED=$(crop_diff /output/beads-drag-classifier-before.png \
    /output/beads-drag-classifier-notice.png \
    "${WIN_W}x40+0+$(( STRIP_TOP + A2_STRIP_H + 14 ))")
[ "${NOTICE_CHANGED:-0}" -ge 1000 ] \
    || fail "classifier-won notice changed only ${NOTICE_CHANGED:-0}px"

# Same-lane, derived-lane, and collapsed-Blocked drops never enter the write
# queue or touch bd. The Blocked arm is a drop on the painted 36px rail tab,
# which is the only Blocked target this board has.
REJECT_BEFORE=$(issue_write_count e2e-close)
drag_issue e2e-close 1
drag_issue e2e-close 0
drag_issue e2e-close 3
sleep 0.8
[ "$(issue_write_count e2e-close)" -eq "$REJECT_BEFORE" ] \
    || fail "rejected or no-op drop queued an issue write"
REJECTED=$(cd "$PROJECT" && bd show e2e-close --json)
printf '%s\n' "$REJECTED" | grep -Fq '"status": "open"' \
    || fail "rejected or no-op drop changed persisted state: $REJECTED"

import -window "$WID" /output/beads-drag-functional.png
echo 'PASS: real bd detail and card drags proved claim, close/Undo, clear-defer,' \
    'classifier notice, rejects, and PTY mouse isolation'

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Collapsed rail drawer, pin, and pinned drop]]
# The rail is where A2 stopped being five equal lanes: Blocked and Done are
# 36px tabs whose drawer, pin, and unpin are pointer and keyboard controls, and
# a pinned lane is a real drop target. Every assertion below reads the painted
# tracks back, so nothing here assumes which queue currently holds which width.
measure_rail /output/beads-rail-idle.png
IDLE_FIRST_TRACK=$(printf '%s\n' "$RAIL_TRACKS" | sed -n 1p)
BLOCKED_TAB_X=$(lane_x 3)
TAB_Y=$(row_y 1)

# The drawer's own painted top border, at the bounds the contract fixes it to.
# A pinned lane is never a drawer, so finding this border is also how an unpin
# that leaves the tab focused proves itself.
drawer_border() {
    python3 /tests/func/beads-board-geometry.py run \
        --shot "$1" --width "$WIN_W" --y "$(( STRIP_TOP + A2_DRAWER_TOP ))" --height 2 \
        --min-width "$(( A2_DRAWER_W - 24 ))"
}

assert_drawer_open() {
    local shot="$1" reason="$2" bounds left expected
    import -window "$WID" "$shot"
    bounds=$(drawer_border "$shot") || fail "$reason"
    left=${bounds%% *}
    expected=$(( WIN_W - A2_DRAWER_RIGHT - A2_DRAWER_W ))
    [ "$left" -ge "$(( expected - 2 ))" ] && [ "$left" -le "$(( expected + 4 ))" ] \
        || fail "$reason (a ${bounds} run is not the drawer's ${expected}px left edge)"
}

assert_drawer_closed() {
    import -window "$WID" "$1"
    if drawer_border "$1" >/dev/null 2>&1; then
        fail "$2"
    fi
}

assert_drawer_closed /output/beads-drawer-idle.png "an untouched rail already showed a drawer"

# Hover opens the transient drawer over the lanes without moving them.
xdotool mousemove --sync --window "$WID" "$BLOCKED_TAB_X" "$TAB_Y"
sleep 0.6
assert_drawer_open /output/beads-drawer-hover.png "hovering the Blocked tab opened no drawer"
HOVER_REPORT=$(rail_report /output/beads-drawer-hover.png) \
    || fail "the hovered rail lost its seam row"
[ "$(printf '%s\n' "$HOVER_REPORT" | sed -n 2p)" = "$IDLE_FIRST_TRACK" ] \
    || fail "the drawer reflowed the lanes it opened over"

# Crossing from the tab into the drawer it opened keeps it open, and the board
# under it stays painted: the drawer `occlude`s, so the board only knows the
# pointer is still on it because the drawer says so.
xdotool mousemove --sync --window "$WID" "$(drawer_center_x)" "$TAB_Y"
sleep 0.6
assert_drawer_open /output/beads-drawer-transfer.png \
    "the drawer closed while the pointer crossed into it"
rail_report /output/beads-drawer-inside.png >/dev/null \
    || fail "the strip vanished under the pointer that entered its own drawer"

# Back out to the tab and in again: one pointer move drives two hover sources,
# and the grace period is what keeps that round trip from closing the drawer.
xdotool mousemove --sync --window "$WID" "$BLOCKED_TAB_X" "$TAB_Y"
sleep 0.6
assert_drawer_open /output/beads-drawer-returned.png \
    "returning to the tab closed the drawer it opened"

# Escape closes a transient drawer and nothing else.
xdotool key --clearmodifiers Escape
sleep 0.6
assert_drawer_closed /output/beads-drawer-escaped.png "Escape left the transient drawer open"

# `click to pin` means inside the drawer too, which is only reachable because
# crossing into it keeps both the drawer and the board alive. The target is the
# drawer's own head, beside that hint: its rows are separate click targets that
# open an issue, exactly as rows in a lane do.
#
# Escape left the pointer on the tab, and hover is edge-triggered: re-issuing a
# move to the coordinates it already holds produces no event and reopens
# nothing. Leave the tab first, then enter it again.
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
xdotool mousemove --sync --window "$WID" "$BLOCKED_TAB_X" "$TAB_Y"
sleep 0.6
assert_drawer_open /output/beads-drawer-reopened.png \
    "re-entering the tab after Escape opened no drawer"
xdotool mousemove --sync --window "$WID" \
    "$(drawer_center_x)" "$(( STRIP_TOP + A2_DRAWER_TOP + A2_HEAD_H / 2 ))"
sleep 0.6
xdotool click 1
sleep 0.6
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
measure_rail /output/beads-rail-pinned.png
[ "$(track_field 3 2)" -gt "$A2_TAB_W" ] \
    || fail "clicking inside the drawer did not pin its lane ($(track_field 3 2)px)"
# Exactly one of Blocked and Done is ever pinned, so the other is still a tab.
[ "$(track_field 4 2)" -eq "$A2_TAB_W" ] \
    || fail "pinning Blocked also widened Done ($(track_field 4 2)px)"

# Pinning the other replaces it rather than adding a second pinned lane.
pin_lane 4
[ "$(track_field 3 2)" -eq "$A2_TAB_W" ] \
    || fail "a second pinned lane survived beside Done ($(track_field 3 2)px)"

# That click left the pinned lane's control focused, and it is the same handle
# the tab carries: Enter unpins, and the focus it leaves behind opens the very
# drawer pointer hover opened above.
xdotool key --clearmodifiers Return
sleep 0.6
assert_drawer_open /output/beads-drawer-focused.png \
    "Enter on the pinned lane control did not unpin it into a focus-opened drawer"

# Enter on that still-focused tab pins it again: the keyboard equivalent of the
# click, and the state the pinned-lane drop below needs.
xdotool key --clearmodifiers Return
sleep 0.6
measure_rail /output/beads-rail-key-pinned.png
[ "$(track_field 4 2)" -gt "$A2_TAB_W" ] \
    || fail "Enter on the focused tab did not pin it ($(track_field 4 2)px)"

# A pinned lane is a real drop target: the same guarded close the collapsed tab
# took above, through a track that did not exist two frames ago.
PINNED_CLOSE_BEFORE=$(issue_write_count e2e-deferred close_issue)
PINNED_APPLIED_BEFORE=$(issue_applied_count e2e-deferred)
drag_issue e2e-deferred 4
wait_for_write e2e-deferred close_issue "$PINNED_CLOSE_BEFORE"
wait_for_applied e2e-deferred "$PINNED_APPLIED_BEFORE"
PINNED_CLOSED=$(cd "$PROJECT" && bd show e2e-deferred --json)
printf '%s\n' "$PINNED_CLOSED" | grep -Fq '"status": "closed"' \
    || fail "a drop on the pinned Done lane did not close through bd: $PINNED_CLOSED"

# The pinned head's own `×` unpins by pointer. Dropping keyboard focus
# afterwards is what closes the drawer that focus would otherwise hold open.
unpin_lane 4
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( HEIGHT - 80 ))"
xdotool click 1
sleep 0.4
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.8
measure_rail /output/beads-rail-restored.png
[ "$(track_field 3 2)" -eq "$A2_TAB_W" ] && [ "$(track_field 4 2)" -eq "$A2_TAB_W" ] \
    || fail "the pinned lane's x did not return the rail to two collapsed tabs: $RAIL_TRACKS"
echo 'PASS: the collapsed rail opened by hover and focus, pinned and unpinned by pointer' \
    'and keyboard, kept one pinned lane, and closed a real card through it'

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Keyboard card move]]
# A2-I6 through the shipped client: Space grabs an eligible row, Left/Right
# step the named targets, Enter drops through the same guarded verb a pointer
# drop uses, Enter on a rejected target writes nothing, and Escape cancels.
# Nothing may reach the PTY while a move is armed.
#
# A row click deliberately does not take focus -- the row's own mouse-down
# stops propagation before GPUI's focus transfer, and A2-I4 asks a click to
# open the detail, not to focus. The rail tab does focus itself on click, and
# it is painted immediately after the last In-progress row, so one Shift+Tab
# from it is a deterministic way in for a keyboard-only user.
lane_card_count() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys

snapshot = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    state = message.get("state", {})
    if row.get("dir") == "server" and message.get("type") == "BeadsBoard":
        ready = state.get("Ready") if isinstance(state, dict) else None
        if isinstance(ready, dict):
            snapshot = ready.get("snapshot")
if not isinstance(snapshot, dict):
    raise SystemExit(1)
print(len(snapshot.get(sys.argv[2], [])))
PY
}

xdotool key --clearmodifiers Escape
sleep 0.3
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
measure_rail /output/beads-key-move-lanes.png
[ "$(issue_position e2e-ready)" = "2 0" ] \
    || fail "the keyboard move expected e2e-ready first in In progress, got $(issue_position e2e-ready)"
[ "$(lane_card_count in_progress)" -eq 1 ] \
    || fail "In progress holds $(lane_card_count in_progress) rows, so Shift+Tab has no single target"
pin_lane 3
KEY_INPUT_BEFORE=$(key_input_count)
xdotool key --clearmodifiers shift+Tab
sleep 0.4

# Right from In progress names Blocked, which refuses the move: the drop is
# still made, and it must leave no write behind.
REJECT_KEY_BEFORE=$(issue_write_count e2e-ready)
xdotool key --clearmodifiers space
sleep 0.3
xdotool key --clearmodifiers Right
sleep 0.3
xdotool key --clearmodifiers Return
sleep 0.8
[ "$(issue_write_count e2e-ready)" -eq "$REJECT_KEY_BEFORE" ] \
    || fail "a keyboard drop on the collapsed Blocked tab still wrote"

# Left names Ready, which accepts through the same guarded verb the pointer
# drop used for the same target.
KEY_STATUS_BEFORE=$(issue_write_count e2e-ready set_status)
KEY_APPLIED_BEFORE=$(issue_applied_count e2e-ready)
xdotool key --clearmodifiers space
sleep 0.3
xdotool key --clearmodifiers Left
sleep 0.3
xdotool key --clearmodifiers Return
wait_for_write e2e-ready set_status "$KEY_STATUS_BEFORE"
wait_for_applied e2e-ready "$KEY_APPLIED_BEFORE"
wait_for_lane e2e-ready 1
KEY_MOVED=$(cd "$PROJECT" && bd show e2e-ready --json)
printf '%s\n' "$KEY_MOVED" | grep -Fq '"status": "open"' \
    || fail "the keyboard drop on Ready did not reopen through native bd: $KEY_MOVED"

# Escape cancels the next move with no write at all.
ESCAPE_BEFORE=$(issue_write_count e2e-ready)
xdotool key --clearmodifiers space
sleep 0.3
xdotool key --clearmodifiers Right
sleep 0.3
xdotool key --clearmodifiers Escape
sleep 0.8
[ "$(issue_write_count e2e-ready)" -eq "$ESCAPE_BEFORE" ] \
    || fail "Escape on an armed keyboard move still wrote"
[ "$(key_input_count)" -eq "$KEY_INPUT_BEFORE" ] \
    || fail "the keyboard move leaked keystrokes to the pane program"
CANCELLED=$(cd "$PROJECT" && bd show e2e-ready --json)
printf '%s\n' "$CANCELLED" | grep -Fq '"status": "open"' \
    || fail "the cancelled keyboard move changed persisted state: $CANCELLED"

# Leave the rail as the pointer phases found it: unpin, then drop focus so the
# tab stops holding its drawer open.
unpin_lane 3
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( HEIGHT - 80 ))"
xdotool click 1
sleep 0.4
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.8
measure_rail /output/beads-key-move-restored.png
[ "$(track_field 3 2)" -eq "$A2_TAB_W" ] \
    || fail "the keyboard phase left Blocked pinned: $RAIL_TRACKS"
echo 'PASS: the keyboard move stepped named targets, dropped through the guarded write path,' \
    'refused Blocked, and cancelled clean with no keystroke reaching the pane'

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Board text scale]]
# The steppers live in the strip's own left control gutter now. Scaling text
# repaints every track and row without moving the strip the board reserved.
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
measure_rail /output/beads-scale-before.png
SCALE_STRIP_TOP=$STRIP_TOP
SCALE_CROP=$(strip_crop)
LARGER_X=$(( A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W / 2 ))
SMALLER_X=$(( A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W + A2_ZOOM_GAP + A2_ZOOM_GLYPH_W / 2 ))
ZOOM_Y=$(( STRIP_TOP + A2_ZOOM_TOP + A2_ZOOM_GLYPH_H / 2 ))
for _ in 1 2; do
    xdotool mousemove --sync --window "$WID" "$LARGER_X" "$ZOOM_Y"
    xdotool click 1
    sleep 0.4
done
measure_rail /output/beads-scale-larger.png
[ "$STRIP_TOP" -eq "$SCALE_STRIP_TOP" ] \
    || fail "larger board text moved the strip from $SCALE_STRIP_TOP to $STRIP_TOP"
SCALE_DIFF=$(crop_diff /output/beads-scale-before.png /output/beads-scale-larger.png "$SCALE_CROP")
[ "${SCALE_DIFF:-0}" -ge 2000 ] \
    || fail "the larger-text stepper changed only ${SCALE_DIFF:-0}px"
for _ in 1 2; do
    xdotool mousemove --sync --window "$WID" "$SMALLER_X" "$ZOOM_Y"
    xdotool click 1
    sleep 0.4
done
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
measure_rail /output/beads-scale-restored.png
[ "$STRIP_TOP" -eq "$SCALE_STRIP_TOP" ] \
    || fail "restoring board text moved the strip from $SCALE_STRIP_TOP to $STRIP_TOP"
RESTORED_DIFF=$(crop_diff /output/beads-scale-before.png /output/beads-scale-restored.png \
    "$SCALE_CROP")
[ "${RESTORED_DIFF:-9999}" -le 600 ] \
    || fail "the smaller-text stepper left ${RESTORED_DIFF:-0}px of the larger scale behind"
echo 'PASS: the board text steppers rescaled the rail and restored it without moving the strip'

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Flow view entry and retarget]]
# Everything above has finished with the single-region board it was written
# for, so the Flow epic is only seeded into the painted workspace now. The
# server-owned admission decision is already proven above; what is left is the
# interaction the clarified model turns on — the panel opening while the strip
# alone swaps, and a node moving the panel without moving the epic.
rm -f /usr/local/bin/bd
mv /usr/local/bin/bd-real /usr/local/bin/bd
xdotool key --clearmodifiers Escape
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5

# Five ranks, so the graph is wider than the strip and its wheel travel, edge
# fades, and position bar are real rather than degenerate.
(
    cd "$PROJECT"
    bd create 'Painted Flow epic' --id e2e-flow-epic --type epic --priority 2 >/dev/null
    for issue_id in e2e-flow-a e2e-flow-b e2e-flow-c e2e-flow-d e2e-flow-e e2e-flow-f; do
        bd create "Painted $issue_id" --id "$issue_id" --type task --priority 2 >/dev/null
        bd update "$issue_id" --parent e2e-flow-epic >/dev/null
    done
    bd close e2e-flow-a --reason 'Satisfied painted blocker.' >/dev/null
    bd dep add e2e-flow-b e2e-flow-a >/dev/null
    bd dep add e2e-flow-c e2e-flow-a >/dev/null
    bd dep add e2e-flow-d e2e-flow-b >/dev/null
    bd dep add e2e-flow-d e2e-flow-c >/dev/null
    bd dep add e2e-flow-e e2e-flow-d >/dev/null
    bd dep add e2e-flow-f e2e-flow-e >/dev/null
)

# Every detail request for a member of the painted epic other than $1, so a
# node activation can be proven without depending on which node takes focus.
flow_sibling_requests() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
members = {
    "e2e-flow-a", "e2e-flow-b", "e2e-flow-c", "e2e-flow-d", "e2e-flow-e", "e2e-flow-f",
} - {sys.argv[2]}
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "RequestBeadsIssueDetail"
            and message.get("issue_id") in members):
        count += 1
print(count)
PY
}

epic_graph_requests() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "RequestBeadsEpicGraph"
            and message.get("epic_id") == sys.argv[2]):
        count += 1
print(count)
PY
}

epic_graph_admitted() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server"
            and message.get("type") == "BeadsEpicGraph"
            and message.get("epic_id") == sys.argv[2]
            and isinstance(message.get("outcome"), dict)
            and "graph" in message["outcome"]):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

click_card() {
    local issue="$1" position lane index
    for _ in $(seq 1 50); do
        position=$(issue_position "$issue" 2>/dev/null || true)
        [ -n "$position" ] && break
        sleep 0.2
    done
    [ -n "${position:-}" ] || fail "$issue never painted a card"
    read -r lane index <<<"$position"
    [ "$index" -lt "$A2_BODY_ROWS" ] \
        || fail "$issue sits past the last whole painted row of its lane"
    measure_rail
    xdotool mousemove --sync --window "$WID" "$(lane_x "$lane")" "$(row_y "$index")"
    xdotool mousedown 1
    xdotool mouseup 1
}

# Wait for the seeded epic to reach the painted board before touching it.
#
# Two things gate that, and a fixed-duration sleep satisfies neither. The seed
# went through the `bd` CLI, which the server never observes, so its board
# cache only notices on the next expiry -- a full CACHE_TTL, not a poll. And
# the client only asks for a board when the pointer actually moves: badge hover
# is edge-triggered, so re-issuing `mousemove` to the coordinates the pointer
# already occupies produces no event and therefore no request. Parking on the
# badge waits forever. Move the pointer away and back each round so every cycle
# is a real hover, and keep going long enough to outlast the cache.
wait_for_seeded_card() {
    local issue="$1" deadline=$(( SECONDS + 120 ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        issue_position "$issue" >/dev/null 2>&1 && return 0
        xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( HEIGHT - 80 ))"
        sleep 0.3
        xdotool mousemove --sync --window "$WID" 13 17
        sleep 1.7
    done
    fail "$issue never reached the painted board after seeding"
}
# The deepest members are blocked, and Blocked is a collapsed tab with no rows
# to click: the entry card is the epic member the board actually paints.
wait_for_seeded_card e2e-flow-b
measure_rail /output/beads-flow-lanes.png

FLOW_DETAIL_BEFORE=$(flow_detail_requests e2e-flow-b)
FLOW_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
click_card e2e-flow-b
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests e2e-flow-b)" -gt "$FLOW_DETAIL_BEFORE" ] \
        && [ "$(epic_graph_requests e2e-flow-epic)" -gt "$FLOW_GRAPH_BEFORE" ] \
        && break
    sleep 0.2
done
# One click owes both: the panel is untouched by Flow and opens as it always
# has, and only the strip follows the card into its epic.
[ "$(flow_detail_requests e2e-flow-b)" -gt "$FLOW_DETAIL_BEFORE" ] \
    || fail "the card click did not open the detail panel"
[ "$(epic_graph_requests e2e-flow-epic)" -gt "$FLOW_GRAPH_BEFORE" ] \
    || fail "the card click did not ask for its epic graph"
for _ in $(seq 1 50); do
    epic_graph_admitted e2e-flow-epic && break
    sleep 0.2
done
epic_graph_admitted e2e-flow-epic \
    || fail "the server never admitted the painted epic's graph"
sleep 0.5
import -window "$WID" /output/beads-flow-strip.png
STRIP_DIFF=$(crop_diff /output/beads-flow-lanes.png /output/beads-flow-strip.png "$(strip_crop)")
[ "${STRIP_DIFF:-0}" -ge 5000 ] \
    || fail "the strip did not repaint into Flow (${STRIP_DIFF:-0}px)"

# The panel begins below the titlebar plus Flow's own strip reservation.
# Diffing with that strip masked isolates its real painted surface.
read -r FLOW_PANEL_W FLOW_PANEL_H FLOW_PANEL_X FLOW_PANEL_Y \
    <<<"$(panel_bounds /output/beads-flow-lanes.png /output/beads-flow-strip.png)"
FLOW_PANEL_TOP=${FLOW_PANEL_Y#+}
[ "${FLOW_PANEL_TOP:-0}" -ge "$(( STRIP_TOP + A2_STRIP_H ))" ] \
    || fail "Flow detail panel overlaps the strip at y=${FLOW_PANEL_Y:-unknown}"

# A real click on rank 0 must cross the panel layer and reach the node handler.
# Its coordinates are the contract's own A3 formulas -- the node box at the
# graph's left padding, its single-row rank centred in the fixed graph band
# under the measured strip top, the Flow band, and the rank ruler.
FLOW_ROOT_X=$(( A3_LEFT_PAD + A3_NODE_W / 2 ))
FLOW_GRAPH_MID_Y=$(( STRIP_TOP + A3_GRAPH_TOP + A3_GRAPH_H / 2 ))
FLOW_ROOT_Y=$FLOW_GRAPH_MID_Y
FLOW_GUTTER_X=$(( A3_LEFT_PAD + A3_NODE_W + (A3_RANK_PITCH - A3_NODE_W) / 2 ))
FLOW_GRAPH_CROP="${WIN_W}x${A3_GRAPH_H}+0+$(( STRIP_TOP + A3_GRAPH_TOP ))"
ROOT_DETAIL_BEFORE=$(flow_detail_requests e2e-flow-a)
GRAPH_AFTER_OPEN=$(epic_graph_requests e2e-flow-epic)
xdotool mousemove --sync --window "$WID" "$FLOW_ROOT_X" "$FLOW_ROOT_Y"
xdotool click 1
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests e2e-flow-a)" -gt "$ROOT_DETAIL_BEFORE" ] && break
    sleep 0.2
done
[ "$(flow_detail_requests e2e-flow-a)" -gt "$ROOT_DETAIL_BEFORE" ] \
    || fail "clicking a Flow node did not retarget the detail panel"
[ "$(epic_graph_requests e2e-flow-epic)" -eq "$GRAPH_AFTER_OPEN" ] \
    || fail "a Flow node click re-requested the epic graph"

# The click focuses rank 0, so Tab reaches another node and Enter activates it.
# This proves the rendered controls stay in the panel's focus cycle after the
# retarget refresh rather than merely exposing an AccessKit role.
TAB_DETAIL_BEFORE=$(flow_sibling_requests e2e-flow-a)
xdotool key --clearmodifiers Tab
sleep 0.2
xdotool key --clearmodifiers Return
for _ in $(seq 1 50); do
    [ "$(flow_sibling_requests e2e-flow-a)" -gt "$TAB_DETAIL_BEFORE" ] && break
    sleep 0.2
done
[ "$(flow_sibling_requests e2e-flow-a)" -gt "$TAB_DETAIL_BEFORE" ] \
    || fail "Tab and Enter did not activate another Flow node"
[ "$(epic_graph_requests e2e-flow-epic)" -eq "$GRAPH_AFTER_OPEN" ] \
    || fail "keyboard Flow activation re-requested the epic graph"

# Hover traces the path through the node under the pointer and dims the rest;
# leaving restores every node and wire.
xdotool mousemove --sync --window "$WID" "$FLOW_GUTTER_X" "$FLOW_GRAPH_MID_Y"
sleep 0.5
import -window "$WID" /output/beads-flow-untraced.png
xdotool mousemove --sync --window "$WID" "$FLOW_ROOT_X" "$FLOW_ROOT_Y"
sleep 0.5
import -window "$WID" /output/beads-flow-traced.png
TRACE_DIFF=$(crop_diff /output/beads-flow-untraced.png /output/beads-flow-traced.png \
    "$FLOW_GRAPH_CROP")
[ "${TRACE_DIFF:-0}" -ge 1000 ] \
    || fail "hovering a Flow node traced nothing (${TRACE_DIFF:-0}px)"
xdotool mousemove --sync --window "$WID" "$FLOW_GUTTER_X" "$FLOW_GRAPH_MID_Y"
sleep 0.5
import -window "$WID" /output/beads-flow-untraced-again.png
UNTRACE_DIFF=$(crop_diff /output/beads-flow-untraced.png /output/beads-flow-untraced-again.png \
    "$FLOW_GRAPH_CROP")
[ "${UNTRACE_DIFF:-0}" -le 400 ] \
    || fail "leaving the node left ${UNTRACE_DIFF:-0}px of trace behind"

# Five ranks are wider than this strip, so the position bar exists and the
# wheel travels the graph. Its own painted mark is the oracle for travel and
# for both clamps; the wheel is claimed by Flow, so the pane sees nothing.
position_mark() {
    import -window "$WID" "$1"
    python3 /tests/func/beads-board-geometry.py run \
        --shot "$1" --width "$WIN_W" --y "$(( STRIP_TOP + A3_HBAR_TOP ))" --height "$A3_HBAR_H" \
        --min-width 20
}

wheel_flow() {
    xdotool mousemove --sync --window "$WID" "$FLOW_GUTTER_X" "$FLOW_GRAPH_MID_Y"
    # Let the client consume the move before the first wheel, and space the
    # clicks: a wheel event is dispatched against the hit test the last
    # processed pointer position produced, and a burst tighter than the
    # client's frame arrives as one coalesced delta rather than the separate
    # notches a reader actually turns.
    sleep 0.4
    xdotool click --repeat "$1" --delay 150 "$2"
    sleep 0.5
}

MARK_START=$(position_mark /output/beads-flow-scroll-origin.png) \
    || fail "an overflowing Flow graph painted no position bar"
WHEEL_REPORTS_BEFORE=$(wheel_report_count)
wheel_flow 5 5
MARK_MOVED=$(position_mark /output/beads-flow-scrolled.png) \
    || fail "the position bar vanished while scrolling"
[ "${MARK_MOVED%% *}" -ne "${MARK_START%% *}" ] \
    || fail "the wheel moved no position (${MARK_START} -> ${MARK_MOVED})"
# A wheel over Flow is Flow's, travelling or clamped: it never reaches the
# pane behind it. The clamped ends are bracketed too, below, because that is
# the half that once handed the pane a wheel it had already handled.
[ "$(wheel_report_count)" -eq "$WHEEL_REPORTS_BEFORE" ] \
    || fail "a travelling wheel over Flow leaked wheel reports to the pane"
SCROLL_DIFF=$(crop_diff /output/beads-flow-scroll-origin.png /output/beads-flow-scrolled.png \
    "$FLOW_GRAPH_CROP")
[ "${SCROLL_DIFF:-0}" -ge 2000 ] \
    || fail "the wheel moved the position bar but not the graph (${SCROLL_DIFF:-0}px)"
wheel_flow 10 5
MARK_CLAMPED=$(position_mark /output/beads-flow-scroll-end.png) \
    || fail "the position bar vanished at the far end"
wheel_flow 3 5
MARK_STILL_CLAMPED=$(position_mark /output/beads-flow-scroll-clamped.png) \
    || fail "the position bar vanished past the far end"
[ "${MARK_CLAMPED%% *}" -eq "${MARK_STILL_CLAMPED%% *}" ] \
    || fail "scrolling past the end kept moving (${MARK_CLAMPED} -> ${MARK_STILL_CLAMPED})"
wheel_flow 20 4
MARK_HOME=$(position_mark /output/beads-flow-scroll-home.png) \
    || fail "the position bar vanished back at the origin"
[ "${MARK_HOME%% *}" -eq "${MARK_START%% *}" ] \
    || fail "scrolling back did not clamp at the origin (${MARK_START} -> ${MARK_HOME})"
[ "$(wheel_report_count)" -eq "$WHEEL_REPORTS_BEFORE" ] \
    || fail "a clamped wheel over Flow leaked wheel reports to the pane"

# `\u2190 LANES` is a real pointer control: it returns to the lanes, which the
# measurable seam row proves outright.
BACK_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
xdotool mousemove --sync --window "$WID" \
    "$(( A3_BAND_PAD_LEFT + 10 ))" "$(( STRIP_TOP + A3_BAND_H / 2 ))"
sleep 0.3
xdotool click 1
sleep 0.8
measure_rail /output/beads-flow-back-to-lanes.png

# Reopening is the refresh action: the frozen graph is dropped on exit, so a
# second entry asks the server for a complete one again.
click_card e2e-flow-c
for _ in $(seq 1 50); do
    [ "$(epic_graph_requests e2e-flow-epic)" -gt "$BACK_GRAPH_BEFORE" ] && break
    sleep 0.2
done
[ "$(epic_graph_requests e2e-flow-epic)" -gt "$BACK_GRAPH_BEFORE" ] \
    || fail "reopening Flow reused the graph it left instead of requesting a fresh one"

# The strip swaps on the server's answer, not on the request, and that answer
# is a fresh `bd` run: wait for the reopened graph's own position bar rather
# than for a fixed interval that a loaded host outlasts.
for _ in $(seq 1 60); do
    position_mark /output/beads-flow-reopened.png >/dev/null 2>&1 && break
    sleep 0.3
done
position_mark /output/beads-flow-reopened.png >/dev/null 2>&1 \
    || fail "reopening Flow never painted its graph"

# The mode pair's `LANES` member is the second real exit. `FLOW` beside it is
# the only selected-state chip in the band, so its painted left edge is what
# locates `LANES` without guessing a text width.
FLOW_CHIP=$(python3 /tests/func/beads-board-geometry.py run \
    --shot /output/beads-flow-reopened.png --width "$WIN_W" \
    --y "$(( STRIP_TOP + A3_BAND_H / 4 ))" --height 4 --min-width 20) \
    || fail "the reopened Flow band painted no selected FLOW chip"
xdotool mousemove --sync --window "$WID" \
    "$(( ${FLOW_CHIP%% *} - 5 ))" "$(( STRIP_TOP + A3_BAND_H / 2 ))"
sleep 0.3
xdotool click 1
sleep 0.8
measure_rail /output/beads-flow-mode-exit.png

# Leaving Flow returns the same board: lanes paint again and a card with no
# epic opens its panel without ever asking for a graph. Select the current
# snapshot's first epic-less whole-row card, not one fixture's old position:
# Flow's two Ready cards may fill that lane before this closing check runs.
loose_card() {
    python3 - "$RECORD" "$A2_BODY_ROWS" <<'PY'
import json, sys

snapshot = None
for line in open(sys.argv[1]):
    try:
        row = json.loads(line)
    except ValueError:
        continue
    message = row.get("message", {})
    state = message.get("state", {})
    if row.get("dir") == "server" and message.get("type") == "BeadsBoard":
        ready = state.get("Ready") if isinstance(state, dict) else None
        if isinstance(ready, dict):
            snapshot = ready.get("snapshot")
if not isinstance(snapshot, dict):
    raise SystemExit(1)
for lane in ("backlog", "ready", "in_progress"):
    for card in snapshot.get(lane, [])[:int(sys.argv[2])]:
        if card.get("parent_epic_id") is None:
            print(card["id"])
            raise SystemExit
raise SystemExit(1)
PY
}

LOOSE_CARD=$(loose_card) \
    || fail "no epic-less fixture is on a painted row for the return-to-lanes check"
LOOSE_DETAIL_BEFORE=$(flow_detail_requests "$LOOSE_CARD")
LOOSE_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
click_card "$LOOSE_CARD"
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests "$LOOSE_CARD")" -gt "$LOOSE_DETAIL_BEFORE" ] && break
    sleep 0.2
done
[ "$(flow_detail_requests "$LOOSE_CARD")" -gt "$LOOSE_DETAIL_BEFORE" ] \
    || fail "lanes stopped opening the panel after a Flow round trip"
[ "$(epic_graph_requests e2e-flow-epic)" -eq "$LOOSE_GRAPH_BEFORE" ] \
    || fail "a card with no epic asked for a graph"

import -window "$WID" /output/beads-flow-functional.png
echo 'PASS: a real card click opened the panel and swapped the strip into Flow, a node' \
    'retargeted the panel inside the frozen epic, hover traced it, the wheel travelled and' \
    'clamped it, both exit controls returned to lanes, and reopening asked for a fresh graph'

# ---- Lifetime phases: liveness, board resize, two-region isolation ---------
# These three read real controls, the hook wire, and the window geometry record
# rather than the pixels the visual matrix owns. Each one sets up the board
# state it needs and hands the board back the way it found it, so none of them
# inherits a pin, a hook binding, or a focus from the phase before it.
record_lines() {
    wc -l <"$RECORD"
}

primary_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if row.get("dir") == "client" and message.get("type") == "CreateSession":
        print(message["workspace_id"])
        raise SystemExit
raise SystemExit(1)
PY
}

session_after() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
workspace, after = sys.argv[2], int(sys.argv[3])
for line_number, line in enumerate(open(sys.argv[1]), 1):
    if line_number <= after:
        continue
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "SessionCreated"
            and message.get("workspace_id") == workspace):
        print(message["session_id"])
        raise SystemExit
raise SystemExit(1)
PY
}

session_exited() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "SessionExited"
            and message.get("session_id") == sys.argv[2]):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

other_workspace() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
primary = sys.argv[2]
for line in reversed(open(sys.argv[1]).readlines()):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "WorkspaceInfo"
            and message.get("workspace_id") != primary):
        print(message["workspace_id"])
        raise SystemExit
raise SystemExit(1)
PY
}


focused_wire_count() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
session, issue = sys.argv[2:]
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if row.get("dir") != "server" or message.get("type") != "IssueFocused":
        continue
    if message.get("session_id") != session:
        continue
    if (issue == "null" and message.get("issue_id") is None) or message.get("issue_id") == issue:
        count += 1
print(count)
PY
}

wait_focused_wire() {
    local session="$1" issue="$2" expected="$3" label="$4" value
    for _ in $(seq 1 80); do
        value=$(focused_wire_count "$session" "$issue" 2>/dev/null || true)
        [ "$value" = "$expected" ] && return 0
        sleep 0.2
    done
    fail "$label (wanted $expected, saw ${value:-missing})"
}

send_issue_focus() {
    printf '{"issue_id":"%s"}' "$2" | \
        SCRIBE_HOOK_SOCK="$SCRIBE_RUNTIME_DIR/server.sock" SCRIBE_SESSION_ID="$1" \
        scribe-hook-helper --provider=codex_code --event=issue_focused --payload-stdin
}

clear_issue_focus() {
    SCRIBE_HOOK_SOCK="$SCRIBE_RUNTIME_DIR/server.sock" SCRIBE_SESSION_ID="$1" \
        scribe-hook-helper --provider=codex_code --event=state_cleared
}

# Crop one live window region before running the shared pixel geometry helper.
# `rail` deliberately sees one rail, so a split window must never feed it both;
# a single-region phase passes the whole window as its one region.
REGION_LEFT=0
REGION_WIDTH=0
REGION_IMAGE=
REGION_TOP=
REGION_TRACKS=

region_capture() {
    local left="$1" width="$2" shot="$3"
    import -window "$WID" "$shot"
    REGION_LEFT=$left
    REGION_WIDTH=$width
    REGION_IMAGE="${shot%.png}-region.png"
    convert "$shot" -crop "${width}x${HEIGHT}+${left}+0" +repage "$REGION_IMAGE"
}

region_measure() {
    local report tracks
    region_capture "$1" "$2" "$3"
    report=$(python3 /tests/func/beads-board-geometry.py rail \
        --contract "$CONTRACT" --shot "$REGION_IMAGE" --width "$REGION_WIDTH") || return 1
    REGION_TOP=$(printf '%s\n' "$report" | head -1)
    REGION_TRACKS=$(printf '%s\n' "$report" | tail -n +2)
    tracks=$(printf '%s\n' "$REGION_TRACKS" | wc -l)
    [ "$tracks" -eq 5 ]
}

region_track_field() {
    printf '%s\n' "$REGION_TRACKS" | sed -n "$(( $1 + 1 ))p" | cut -d' ' -f"$2"
}

region_lane_x() {
    echo $(( REGION_LEFT + $(region_track_field "$1" 1) + $(region_track_field "$1" 2) / 2 ))
}

region_row_y() {
    echo $(( REGION_TOP + A2_HEADBAND_H + $1 * A2_ROW_H + A2_ROW_H / 2 ))
}


# A3's own painted position bar, the mark that only exists while that region is
# in Flow with a graph wider than its strip.
region_flow_mark() {
    python3 /tests/func/beads-board-geometry.py run \
        --shot "$REGION_IMAGE" --width "$REGION_WIDTH" \
        --y "$(( $1 + A3_HBAR_TOP ))" --height "$A3_HBAR_H" --min-width 20
}

wait_region_rail() {
    local left="$1" width="$2" shot="$3"
    for _ in $(seq 1 80); do
        region_measure "$left" "$width" "$shot" && return 0
        sleep 0.2
    done
    fail "region at x=$left did not paint an A2 rail"
}

wait_region_flow() {
    local left="$1" width="$2" top="$3" shot="$4"
    for _ in $(seq 1 80); do
        region_capture "$left" "$width" "$shot"
        region_flow_mark "$top" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    fail "region at x=$left did not paint an A3 position bar"
}

region_issue_position() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
workspace, issue = sys.argv[2:]
snapshot = None
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    state = message.get("state", {})
    if (row.get("dir") == "server" and message.get("type") == "BeadsBoard"
            and message.get("workspace_id") == workspace):
        ready = state.get("Ready") if isinstance(state, dict) else None
        if isinstance(ready, dict):
            snapshot = ready.get("snapshot")
if not isinstance(snapshot, dict):
    raise SystemExit(1)
for lane, name in enumerate(("backlog", "ready", "in_progress", "blocked", "done")):
    for index, card in enumerate(snapshot.get(name, [])):
        if card.get("id") == issue:
            print(lane, index)
            raise SystemExit
raise SystemExit(1)
PY
}

click_region_card() {
    local workspace="$1" left="$2" width="$3" issue="$4" position lane index
    for _ in $(seq 1 80); do
        position=$(region_issue_position "$workspace" "$issue" 2>/dev/null || true)
        [ -n "$position" ] && break
        sleep 0.2
    done
    [ -n "${position:-}" ] || fail "$issue never reached region $workspace"
    read -r lane index <<<"$position"
    [ "$index" -lt "$A2_BODY_ROWS" ] || fail "$issue is not on a whole visible row in region $workspace"
    wait_region_rail "$left" "$width" /output/beads-region-click.png
    xdotool mousemove --sync --window "$WID" "$(region_lane_x "$lane")" "$(region_row_y "$index")"
    xdotool click 1
}

# The board furniture the window geometry record owns, read back out of the
# file the client actually writes rather than inferred from pixels.
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
geometry_field() {
    python3 - "$STATE_DIR/windows" "$1" "$2" <<'PY'
import pathlib, sys, tomllib
root, field, workspace = sys.argv[1:]
files = sorted(pathlib.Path(root).glob("*.toml"), key=lambda path: path.stat().st_mtime)
if not files:
    raise SystemExit(1)
state = tomllib.loads(files[-1].read_text())
if field == "pinned":
    print(str(workspace in map(str, state.get("beads_pinned", []))).lower())
elif field == "lane":
    for row in state.get("beads_lane_pinned", []):
        if len(row) == 2 and str(row[0]) == workspace:
            print(row[1])
            break
    else:
        print("none")
elif field == "height":
    for row in state.get("beads_heights", []):
        if len(row) == 2 and str(row[0]) == workspace:
            print(row[1])
            break
    else:
        print("default")
elif field == "scale":
    print(state.get("beads_text_scale_steps", 0))
else:
    raise SystemExit(2)
PY
}

wait_geometry() {
    local field="$1" workspace="$2" expected="$3" value
    for _ in $(seq 1 80); do
        value=$(geometry_field "$field" "$workspace" 2>/dev/null || true)
        [ "$value" = "$expected" ] && return 0
        sleep 0.2
    done
    fail "persisted $field for $workspace was ${value:-missing}, not $expected"
}

geometry_tuple() {
    printf '%s %s %s %s\n' \
        "$(geometry_field pinned "$1")" \
        "$(geometry_field lane "$1")" \
        "$(geometry_field height "$1")" \
        "$(geometry_field scale "$1")"
}

refresh_window() {
    eval "$(xdotool getwindowgeometry --shell "$WID")"
    WIN_W=$WIDTH
}

toggle_board_pin() {
    xdotool mousemove --sync --window "$WID" "$(( $1 + 13 ))" 17
    sleep 0.4
    xdotool click 1
    sleep 0.6
}

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Flow liveness lifetime]]
# A real hook owns A3-L3's liveness state. Both session bindings arrive on the
# wire before their halos are compared, so nothing here is an injected
# paint-only capture, and `state_cleared` is the real lifecycle hook that ends
# a session's claim on an issue.
PRIMARY_WORKSPACE=$(primary_workspace) || fail "client wire carried no primary workspace"
FIRST_SESSION=$(session_after "$PRIMARY_WORKSPACE" 0) || fail "client wire carried no primary session"

LIVE_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
click_card e2e-flow-b
for _ in $(seq 1 50); do
    [ "$(epic_graph_requests e2e-flow-epic)" -gt "$LIVE_GRAPH_BEFORE" ] && break
    sleep 0.2
done
[ "$(epic_graph_requests e2e-flow-epic)" -gt "$LIVE_GRAPH_BEFORE" ] \
    || fail "the liveness phase did not reopen Flow"
# The halo baseline has to be a painted graph, not the lanes the strip is still
# showing while the server runs `bd` for the reopened epic.
for _ in $(seq 1 60); do
    position_mark /output/beads-live-clear.png >/dev/null 2>&1 && break
    sleep 0.3
done
position_mark /output/beads-live-clear.png >/dev/null 2>&1 \
    || fail "the liveness phase never painted its Flow graph"
sleep 0.4
import -window "$WID" /output/beads-live-clear.png
send_issue_focus "$FIRST_SESSION" e2e-flow-b
wait_focused_wire "$FIRST_SESSION" e2e-flow-b 1 "first hook focus never reached the client"
sleep 0.4
import -window "$WID" /output/beads-live-one.png
LIVE_ONE_DIFF=$(crop_diff /output/beads-live-clear.png /output/beads-live-one.png "$FLOW_GRAPH_CROP")
[ "${LIVE_ONE_DIFF:-0}" -ge 80 ] \
    || fail "hook-driven live issue changed only ${LIVE_ONE_DIFF:-0}px"

# A second real session on the same issue. The tab inherits the focused
# session's cwd, so the workspace keeps the very board this run has measured
# all along.
SECOND_START=$(record_lines)
xdotool key --clearmodifiers ctrl+shift+t
for _ in $(seq 1 80); do
    LIVE_SECOND_SESSION=$(session_after "$PRIMARY_WORKSPACE" "$SECOND_START" 2>/dev/null || true)
    [ -n "${LIVE_SECOND_SESSION:-}" ] && break
    sleep 0.2
done
[ -n "${LIVE_SECOND_SESSION:-}" ] || fail "opening a second live tab created no session"
send_issue_focus "$LIVE_SECOND_SESSION" e2e-flow-b
wait_focused_wire "$LIVE_SECOND_SESSION" e2e-flow-b 1 "second hook focus never reached the client"
sleep 0.4
import -window "$WID" /output/beads-live-two.png
clear_issue_focus "$FIRST_SESSION"
wait_focused_wire "$FIRST_SESSION" null 1 "state-cleared hook left the first live binding behind"
sleep 0.4
import -window "$WID" /output/beads-live-one-cleared.png
LIVE_RETAINED_DIFF=$(crop_diff /output/beads-live-two.png /output/beads-live-one-cleared.png \
    "$FLOW_GRAPH_CROP")
[ "${LIVE_RETAINED_DIFF:-9999}" -le 80 ] \
    || fail "clearing one of two live sessions erased ${LIVE_RETAINED_DIFF:-0}px of halo"
clear_issue_focus "$LIVE_SECOND_SESSION"
wait_focused_wire "$LIVE_SECOND_SESSION" null 1 "state-cleared hook left the second live binding behind"
sleep 0.4
import -window "$WID" /output/beads-live-all-cleared.png
LIVE_CLEAR_DIFF=$(crop_diff /output/beads-live-two.png /output/beads-live-all-cleared.png \
    "$FLOW_GRAPH_CROP")
[ "${LIVE_CLEAR_DIFF:-0}" -ge 80 ] \
    || fail "clearing both live sessions removed only ${LIVE_CLEAR_DIFF:-0}px"
echo 'PASS: two hook-backed sessions kept the Flow halo live until both state-cleared hooks ran'

# Hand the board back the way this phase found it: in lanes, on one session.
# The tab strip lives inside the fixed-height titlebar, so closing the tab
# leaves the strip top where the exit control was just measured.
xdotool mousemove --sync --window "$WID" \
    "$(( A3_BAND_PAD_LEFT + 10 ))" "$(( STRIP_TOP + A3_BAND_H / 2 ))"
xdotool click 1
sleep 0.8
measure_rail /output/beads-live-back-to-lanes.png
xdotool key --clearmodifiers ctrl+shift+q
for _ in $(seq 1 80); do
    session_exited "$LIVE_SECOND_SESSION" && break
    sleep 0.2
done
session_exited "$LIVE_SECOND_SESSION" || fail "the liveness phase's second tab never closed"
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.8

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Board resize and text scale lifetime]]
# A2-R1/A2-R2/A2-R3 through the shipped client, with the window geometry record
# as the oracle for what the board persisted and the painted rail as the oracle
# for what it allocated.
#
# A2-R2's threshold is each active lane's own measured legible header width, so
# a lane holding nothing is never starved and an empty Backlog or In-progress
# board can never reach the auto-collapse rule at all. The phases above leave
# both of those lanes empty, so this one seeds its own card into each: without
# that, a stored pin correctly stays an expanded lane at every width this
# window can take, and "the pin collapsed" would be an assertion about the
# fixture rather than about the contract.
xdotool key --clearmodifiers Escape
sleep 0.3
xdotool mousemove --sync --window "$WID" 13 17
sleep 0.5
(
    cd "$PROJECT"
    bd create 'Resize backlog fixture' --id e2e-resize-backlog --type task --priority 2 \
        --defer '2030-05-01' >/dev/null
    bd create 'Resize active fixture' --id e2e-resize-active --type task --priority 2 >/dev/null
    bd update e2e-resize-active --claim >/dev/null
)
wait_for_seeded_card e2e-resize-backlog
wait_for_seeded_card e2e-resize-active
for lane in backlog ready in_progress; do
    [ "$(lane_card_count "$lane")" -gt 0 ] \
        || fail "A2-R2 needs work in every active lane; $lane is empty"
done

RESIZE_ENTRY_W=$WIN_W
RESIZE_ENTRY_H=$HEIGHT
# `MIN_BOARD_W` is the contract's own derived narrow floor (44 + 452 + 96), not
# a breakpoint invented here; the wide width is the mock's A3 viewport.
RESIZE_WIDE_W=1008
RESIZE_NARROW_W=592
xdotool windowsize --sync "$WID" "$RESIZE_WIDE_W" 739
sleep 0.8
refresh_window
[ "$(geometry_field pinned "$PRIMARY_WORKSPACE" 2>/dev/null || true)" = true ] \
    || toggle_board_pin 0
wait_geometry pinned "$PRIMARY_WORKSPACE" true

# A stored pin at full width is a real lane, and only one of Blocked and Done
# ever is.
pin_lane 3
wait_geometry lane "$PRIMARY_WORKSPACE" blocked
wait_region_rail 0 "$WIN_W" /output/beads-resize-wide-pinned.png
RESIZE_STRIP_TOP=$REGION_TOP
[ "$(region_track_field 3 2)" -gt "$A2_TAB_W" ] \
    || fail "the stored Blocked pin was not a lane at ${WIN_W}px ($(region_track_field 3 2)px)"

# A2-R3: text scale recomputes the rail without touching the stored height, so
# the height has to be a non-default one the record actually carries. Drag the
# real floor bar down by exactly one row and read what the client wrote.
RESIZE_HEIGHT=$(( A2_STRIP_H + A2_ROW_H ))
RESIZE_FLOOR_Y=$(( RESIZE_STRIP_TOP + A2_STRIP_H - 1 ))
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$RESIZE_FLOOR_Y"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( RESIZE_FLOOR_Y + A2_ROW_H ))"
xdotool mouseup 1
wait_geometry height "$PRIMARY_WORKSPACE" "$RESIZE_HEIGHT"

# Six `+` steps is the 1.6 ceiling A2-R3 fixes. The zoom glyphs keep the
# contract's own fixed gutter geometry at every scale, so one coordinate drives
# all six clicks.
RESIZE_LARGER_X=$(( A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W / 2 ))
RESIZE_SMALLER_X=$(( A2_ZOOM_LEFT + A2_ZOOM_GLYPH_W + A2_ZOOM_GAP + A2_ZOOM_GLYPH_W / 2 ))
RESIZE_ZOOM_Y=$(( RESIZE_STRIP_TOP + A2_ZOOM_TOP + A2_ZOOM_GLYPH_H / 2 ))
for _ in 1 2 3 4 5 6; do
    xdotool mousemove --sync --window "$WID" "$RESIZE_LARGER_X" "$RESIZE_ZOOM_Y"
    xdotool click 1
    sleep 0.25
done
wait_geometry scale "$PRIMARY_WORKSPACE" 6
wait_region_rail 0 "$WIN_W" /output/beads-resize-wide-scaled.png
[ "$REGION_TOP" -eq "$RESIZE_STRIP_TOP" ] \
    || fail "the 1.6 text scale moved the strip from $RESIZE_STRIP_TOP to $REGION_TOP"
[ "$(geometry_field height "$PRIMARY_WORKSPACE")" = "$RESIZE_HEIGHT" ] \
    || fail "text scale rewrote the stored board height to $(geometry_field height "$PRIMARY_WORKSPACE")"
[ "$(region_track_field 3 2)" -gt "$A2_TAB_W" ] \
    || fail "${WIN_W}px still fits the pinned lane at 1.6, but it collapsed"

# A2-R2 down: at the narrow floor the same 1.6 rail cannot give all three
# active lanes their header width beside a pinned lane, so the pin auto-
# collapses -- and the preference it collapsed from stays in the record.
xdotool windowsize --sync "$WID" "$RESIZE_NARROW_W" 739
sleep 0.8
refresh_window
[ "$WIN_W" -le "$(( RESIZE_NARROW_W + 16 ))" ] \
    || fail "the window manager refused the narrow width and left ${WIN_W}px"
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( HEIGHT - 80 ))"
wait_region_rail 0 "$WIN_W" /output/beads-resize-narrow.png
[ "$(region_track_field 3 2)" -eq "$A2_TAB_W" ] \
    || fail "the starved pin stayed a $(region_track_field 3 2)px lane at ${WIN_W}px"
wait_geometry lane "$PRIMARY_WORKSPACE" blocked
# A2-R1: the tabs keep their fixed geometry and the rail still ends inside the
# board, because A2 answers a narrow region by reallocating, never by scrolling.
[ "$(( $(region_track_field 4 1) + A2_TAB_W ))" -le "$(( WIN_W - A2_LANES_PADDING_RIGHT ))" ] \
    || fail "the narrow rail ran past the board's own right padding: $REGION_TRACKS"

# A2-R2 up: widening restores the lane from the untouched preference, with no
# second click anywhere.
xdotool windowsize --sync "$WID" "$RESIZE_WIDE_W" 739
sleep 0.8
refresh_window
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( HEIGHT - 80 ))"
wait_region_rail 0 "$WIN_W" /output/beads-resize-restored.png
[ "$(region_track_field 3 2)" -gt "$A2_TAB_W" ] \
    || fail "widening back to ${WIN_W}px left the pin collapsed"
wait_geometry height "$PRIMARY_WORKSPACE" "$RESIZE_HEIGHT"
echo 'PASS: the stored lane pin auto-collapsed at the narrow floor and restored on widening,' \
    'while text scale recomputed the rail without moving the strip or the stored height'

# Hand back the default board: 1.0 text, the designed height, no lane pin, no
# board pin, and the window size this phase was handed.
for _ in 1 2 3 4 5 6; do
    xdotool mousemove --sync --window "$WID" "$RESIZE_SMALLER_X" "$RESIZE_ZOOM_Y"
    xdotool click 1
    sleep 0.25
done
wait_geometry scale "$PRIMARY_WORKSPACE" 0
wait_region_rail 0 "$WIN_W" /output/beads-resize-teardown.png
RESIZE_FLOOR_Y=$(( REGION_TOP + RESIZE_HEIGHT - 1 ))
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$RESIZE_FLOOR_Y"
xdotool mousedown 1
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" "$(( RESIZE_FLOOR_Y - A2_ROW_H ))"
xdotool mouseup 1
wait_geometry height "$PRIMARY_WORKSPACE" default
unpin_lane 3
wait_geometry lane "$PRIMARY_WORKSPACE" none
toggle_board_pin 0
wait_geometry pinned "$PRIMARY_WORKSPACE" false
xdotool windowsize --sync "$WID" "$RESIZE_ENTRY_W" "$RESIZE_ENTRY_H"
sleep 0.8
refresh_window

# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Two-region isolation and cleanup]]
# A3-R1 and A2-BD4 need a second real region, which is the only fixture that
# can tell "anchored to its region" from "anchored to the window": at x=0 both
# answers look the same.
region_session() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
workspace = sys.argv[2]
for line in reversed(open(sys.argv[1]).readlines()):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client" and message.get("type") == "MoveSession"
            and message.get("target_workspace") == workspace):
        print(message["session_id"])
        raise SystemExit
    if (row.get("dir") == "server" and message.get("type") == "SessionCreated"
            and message.get("workspace_id") == workspace):
        print(message["session_id"])
        raise SystemExit
raise SystemExit(1)
PY
}

not_detected_seen() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "BeadsBoard"
            and message.get("workspace_id") == sys.argv[2]
            and message.get("state") == "NotDetected"):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

wait_region_cwd() {
    local session="$1" path="$2"
    for _ in $(seq 1 80); do
        python3 - "$RECORD" "$session" "$path" <<'PY' && return 0
import json, sys
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "server" and message.get("type") == "CwdChanged"
            and message.get("session_id") == sys.argv[2]
            and str(message.get("cwd")) == sys.argv[3]):
        raise SystemExit(0)
raise SystemExit(1)
PY
        sleep 0.2
    done
    return 1
}

# A split region is a fresh context with no CWD request of its own, so the
# server's home fallback wins and its board would never be the fixture's. The
# image has no shell integration either, so the region's own live terminal
# emits the OSC 7 the server reads instead.
REGION_CWD_PROBE=/tmp/scribe-region-cwd.sh
cat >"$REGION_CWD_PROBE" <<'SH'
cd "$1" || exit 1
printf '\033]7;file://%s%s\033\\' "${HOSTNAME}" "$PWD"
SH

set_region_cwd() {
    local session="$1" left="$2" path="$3"
    for _ in 1 2 3; do
        # The terminal's exposed lower-left corner, clear of its own board.
        xdotool mousemove --sync --window "$WID" "$(( left + 12 ))" "$(( HEIGHT - 80 ))"
        xdotool click 1
        sleep 1.5
        xdotool type --clearmodifiers --delay 1 -- ". $REGION_CWD_PROBE $path"
        xdotool key --clearmodifiers Return
        wait_region_cwd "$session" "$path" && return 0
    done
    fail "region session $session never changed CWD to $path"
}

xdotool windowsize --sync "$WID" 1008 739
sleep 0.8
refresh_window
HALF=$(( WIN_W / 2 ))
xdotool key --clearmodifiers ctrl+alt+backslash
for _ in $(seq 1 80); do
    SECOND_WORKSPACE=$(other_workspace "$PRIMARY_WORKSPACE" 2>/dev/null || true)
    [ -n "${SECOND_WORKSPACE:-}" ] && break
    sleep 0.2
done
[ -n "${SECOND_WORKSPACE:-}" ] || fail "workspace split created no second region"
for _ in $(seq 1 80); do
    SECOND_SESSION=$(region_session "$SECOND_WORKSPACE" 2>/dev/null || true)
    [ -n "${SECOND_SESSION:-}" ] && break
    sleep 0.2
done
[ -n "${SECOND_SESSION:-}" ] || fail "the second region adopted no session"
set_region_cwd "$SECOND_SESSION" "$HALF" "$PROJECT"

# Both regions pin their own board, so each rail paints without a hover that
# would itself be cross-region state.
[ "$(geometry_field pinned "$PRIMARY_WORKSPACE" 2>/dev/null || true)" = true ] \
    || toggle_board_pin 0
wait_geometry pinned "$PRIMARY_WORKSPACE" true
[ "$(geometry_field pinned "$SECOND_WORKSPACE" 2>/dev/null || true)" = true ] \
    || toggle_board_pin "$HALF"
wait_geometry pinned "$SECOND_WORKSPACE" true
wait_region_rail "$HALF" "$HALF" /output/beads-isolation-right.png
RIGHT_TOP=$REGION_TOP

# A pointer Flow entry in the right region changes nothing about the left one:
# not its persisted furniture, and not one pixel of its painted strip. Park the
# pointer off the left board first, so its own row hover is not the difference.
xdotool mousemove --sync --window "$WID" "$(( HALF / 2 ))" "$(( HEIGHT - 80 ))"
sleep 0.6
wait_region_rail 0 "$HALF" /output/beads-isolation-left-before.png
LEFT_TOP=$REGION_TOP
LEFT_TRACKS=$REGION_TRACKS
LEFT_STRIP_CROP="${HALF}x${A2_STRIP_H}+0+${LEFT_TOP}"
LEFT_BEFORE_FLOW=$(geometry_tuple "$PRIMARY_WORKSPACE")
click_region_card "$SECOND_WORKSPACE" "$HALF" "$HALF" e2e-flow-b
wait_region_flow "$HALF" "$HALF" "$RIGHT_TOP" /output/beads-isolation-right-flow.png
LEFT_AFTER_FLOW=$(geometry_tuple "$PRIMARY_WORKSPACE")
[ "$LEFT_AFTER_FLOW" = "$LEFT_BEFORE_FLOW" ] \
    || fail "right-region Flow changed left board state ($LEFT_BEFORE_FLOW -> $LEFT_AFTER_FLOW)"
xdotool mousemove --sync --window "$WID" "$(( HALF / 2 ))" "$(( HEIGHT - 80 ))"
sleep 0.6
wait_region_rail 0 "$HALF" /output/beads-isolation-left-after.png
[ "$REGION_TRACKS" = "$LEFT_TRACKS" ] \
    || fail "right-region Flow reflowed the left rail: $LEFT_TRACKS -> $REGION_TRACKS"
LEFT_FLOW_DIFF=$(crop_diff /output/beads-isolation-left-before.png \
    /output/beads-isolation-left-after.png "$LEFT_STRIP_CROP")
[ "${LEFT_FLOW_DIFF:-9999}" -le 400 ] \
    || fail "right-region Flow repainted ${LEFT_FLOW_DIFF:-0}px of the left region's strip"

# The reverse direction: a real pin gesture in the left region is that region's
# alone and never exits the other one's Flow.
xdotool mousemove --sync --window "$WID" "$(region_lane_x 3)" "$(region_row_y 1)"
sleep 0.4
xdotool click 1
sleep 0.6
wait_geometry lane "$PRIMARY_WORKSPACE" blocked
region_capture "$HALF" "$HALF" /output/beads-isolation-right-after-left-pointer.png
region_flow_mark "$RIGHT_TOP" >/dev/null \
    || fail "a left-region pointer gesture exited the other region's Flow"

# A2-BD4: a real CWD loss removes only its own region's board, drawer, lane
# pin, and Flow. The active A3 region is deliberately the one that loses it.
PRIMARY_BEFORE_NOT_DETECTED=$(geometry_tuple "$PRIMARY_WORKSPACE")
set_region_cwd "$SECOND_SESSION" "$HALF" /tmp
# The server's board cache is 30s. Re-enter the region badge after that window
# so a real CWD change, not a fixture injection, produces NotDetected.
sleep 31
for _ in $(seq 1 40); do
    xdotool mousemove --sync --window "$WID" "$(( HALF / 2 ))" "$(( HEIGHT - 80 ))"
    xdotool mousemove --sync --window "$WID" "$(( HALF + 13 ))" 17
    not_detected_seen "$SECOND_WORKSPACE" && break
    sleep 0.5
done
not_detected_seen "$SECOND_WORKSPACE" \
    || fail "the real /tmp workspace never returned BeadsBoard NotDetected"
wait_geometry pinned "$SECOND_WORKSPACE" false
[ "$(geometry_tuple "$PRIMARY_WORKSPACE")" = "$PRIMARY_BEFORE_NOT_DETECTED" ] \
    || fail "NotDetected in one region changed its sibling's board state"
xdotool mousemove --sync --window "$WID" "$(( HALF / 2 ))" "$(( HEIGHT - 80 ))"
sleep 1.2
region_capture "$HALF" "$HALF" /output/beads-not-detected-right.png
region_flow_mark "$RIGHT_TOP" >/dev/null 2>&1 \
    && fail "NotDetected left Flow alive in the removed region"
if region_measure "$HALF" "$HALF" /output/beads-not-detected-right-rail.png; then
    fail "NotDetected left the removed region's board painted"
fi
wait_region_rail 0 "$HALF" /output/beads-not-detected-left.png
[ "$(region_track_field 3 2)" -eq "$A2_TAB_W" ] \
    || fail "the left region's starved pin stopped being a tab: $REGION_TRACKS"
echo 'PASS: two regions isolated real pointer, Flow, and NotDetected state while the sibling' \
    'board kept its own pins, height, and text scale'
