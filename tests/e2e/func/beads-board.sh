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

drag_issue() {
    local issue="$1" target_lane="$2" position source_lane index
    local lane_width source_left press_x press_y target_x reports_before reports_after
    # A native release ends the hover overlay. Re-enter through the painted bead
    # target before resolving fresh card geometry for every gesture.
    xdotool mousemove --sync --window "$WID" 13 17
    sleep 0.5
    position=$(issue_position "$issue") || fail "$issue has no painted card"
    read -r source_lane index <<<"$position"
    lane_width=$(( (WIN_W - 16) / 5 ))
    source_left=$(( 16 + source_lane * lane_width ))
    press_x=$(( source_left + 70 ))
    press_y=$(( 70 + index * 50 + 14 ))
    target_x=$(( 16 + target_lane * lane_width + 70 ))
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
DETAIL_POSITION=$(issue_position e2e-detail) || fail "e2e-detail has no painted card"
read -r DETAIL_LANE DETAIL_INDEX <<<"$DETAIL_POSITION"
LANE_WIDTH=$(( (WIN_W - 16) / 5 ))
DETAIL_X=$(( 16 + DETAIL_LANE * LANE_WIDTH + 70 ))
DETAIL_Y=$(( 70 + DETAIL_INDEX * 50 + 14 ))
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
xdotool mousemove --sync --window "$WID" 900 600
sleep 0.5
import -window "$WID" /output/beads-real-detail.png
DETAIL_DIFF=$(compare -metric AE /output/beads-real-board.png \
    /output/beads-real-detail.png null: 2>&1 || true)
DETAIL_DIFF=${DETAIL_DIFF%%.*}
[ "${DETAIL_DIFF:-0}" -ge 20000 ] || fail "real detail panel changed only ${DETAIL_DIFF:-0}px"

panel_bounds() {
    local before="$1" after="$2" content_height=$(( HEIGHT - 58 ))
    convert \
        \( "$before" -crop "${WIN_W}x${content_height}+0+34" \) \
        \( "$after" -crop "${WIN_W}x${content_height}+0+34" \) \
        -compose difference -composite -threshold 10% -trim \
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
sleep 0.5
PROBE_BEFORE=$(mouse_report_count)
xdotool mousemove --sync --window "$WID" "$(( WIN_W / 2 ))" 400
xdotool click 1
for _ in $(seq 1 20); do
    [ "$(mouse_report_count)" -ge "$(( PROBE_BEFORE + 2 ))" ] && break
    sleep 0.2
done
[ "$(mouse_report_count)" -ge "$(( PROBE_BEFORE + 2 ))" ] \
    || fail "owner-visible pane did not enable SGR mouse reporting"
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
NOTICE_CHANGED=$(compare -metric AE \
    \( /output/beads-drag-classifier-before.png -crop "${WIN_W}x40+0+245" +repage \) \
    \( /output/beads-drag-classifier-notice.png -crop "${WIN_W}x40+0+245" +repage \) \
    null: 2>&1 || true)
NOTICE_CHANGED=${NOTICE_CHANGED%%.*}
[ "${NOTICE_CHANGED:-0}" -ge 1000 ] \
    || fail "classifier-won notice changed only ${NOTICE_CHANGED:-0}px"

# Same-lane and derived-lane drops never enter the write queue or touch bd.
REJECT_BEFORE=$(issue_write_count e2e-close)
drag_issue e2e-close 1
drag_issue e2e-close 0
sleep 0.8
[ "$(issue_write_count e2e-close)" -eq "$REJECT_BEFORE" ] \
    || fail "rejected or no-op drop queued an issue write"
REJECTED=$(cd "$PROJECT" && bd show e2e-close --json)
printf '%s\n' "$REJECTED" | grep -Fq '"status": "open"' \
    || fail "rejected or no-op drop changed persisted state: $REJECTED"

import -window "$WID" /output/beads-drag-functional.png
echo 'PASS: real bd detail and card drags proved claim, close/Undo, clear-defer,' \
    'classifier notice, rejects, and PTY mouse isolation'

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

(
    cd "$PROJECT"
    bd create 'Painted Flow epic' --id e2e-flow-epic --type epic --priority 2 >/dev/null
    for issue_id in e2e-flow-a e2e-flow-b e2e-flow-c e2e-flow-d; do
        bd create "Painted $issue_id" --id "$issue_id" --type task --priority 2 >/dev/null
        bd update "$issue_id" --parent e2e-flow-epic >/dev/null
    done
    bd close e2e-flow-a --reason 'Satisfied painted blocker.' >/dev/null
    bd dep add e2e-flow-b e2e-flow-a >/dev/null
    bd dep add e2e-flow-c e2e-flow-a >/dev/null
    bd dep add e2e-flow-d e2e-flow-b >/dev/null
    bd dep add e2e-flow-d e2e-flow-c >/dev/null
)

# Every detail request for a member of the painted epic other than $1, so a
# node activation can be proven without depending on which node takes focus.
flow_sibling_requests() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
members = {"e2e-flow-a", "e2e-flow-b", "e2e-flow-c", "e2e-flow-d"} - {sys.argv[2]}
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
    local issue="$1" position lane index lane_width
    for _ in $(seq 1 50); do
        position=$(issue_position "$issue" 2>/dev/null || true)
        [ -n "$position" ] && break
        sleep 0.2
    done
    [ -n "${position:-}" ] || fail "$issue never painted a card"
    read -r lane index <<<"$position"
    lane_width=$(( (WIN_W - 16) / 5 ))
    xdotool mousemove --sync --window "$WID" \
        "$(( 16 + lane * lane_width + 70 ))" "$(( 70 + index * 50 + 14 ))"
    xdotool mousedown 1
    xdotool mouseup 1
}

# Wait for the seeded epic to reach the painted board before touching it.
for _ in $(seq 1 50); do
    issue_position e2e-flow-d >/dev/null 2>&1 && break
    sleep 0.2
done
import -window "$WID" /output/beads-flow-lanes.png

FLOW_DETAIL_BEFORE=$(flow_detail_requests e2e-flow-d)
FLOW_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
click_card e2e-flow-d
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests e2e-flow-d)" -gt "$FLOW_DETAIL_BEFORE" ] \
        && [ "$(epic_graph_requests e2e-flow-epic)" -gt "$FLOW_GRAPH_BEFORE" ] \
        && break
    sleep 0.2
done
# One click owes both: the panel is untouched by Flow and opens as it always
# has, and only the strip follows the card into its epic.
[ "$(flow_detail_requests e2e-flow-d)" -gt "$FLOW_DETAIL_BEFORE" ] \
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
STRIP_DIFF=$(compare -metric AE \
    \( /output/beads-flow-lanes.png -crop "${WIN_W}x197+0+34" +repage \) \
    \( /output/beads-flow-strip.png -crop "${WIN_W}x197+0+34" +repage \) \
    null: 2>&1 || true)
STRIP_DIFF=${STRIP_DIFF%%.*}
[ "${STRIP_DIFF:-0}" -ge 5000 ] \
    || fail "the strip did not repaint into Flow (${STRIP_DIFF:-0}px)"

# A node moves the opened card inside the frozen graph. The epic must not be
# re-requested: the picture the reader is holding stays exactly as laid out.
SIBLING_BEFORE=$(flow_sibling_requests e2e-flow-d)
GRAPH_AFTER_OPEN=$(epic_graph_requests e2e-flow-epic)
xdotool key --clearmodifiers Tab
sleep 0.2
xdotool key --clearmodifiers Return
for _ in $(seq 1 50); do
    [ "$(flow_sibling_requests e2e-flow-d)" -gt "$SIBLING_BEFORE" ] && break
    sleep 0.2
done
[ "$(flow_sibling_requests e2e-flow-d)" -gt "$SIBLING_BEFORE" ] \
    || fail "activating a Flow node did not retarget the detail panel"
[ "$(epic_graph_requests e2e-flow-epic)" -eq "$GRAPH_AFTER_OPEN" ] \
    || fail "a node activation re-requested the epic graph"

# Leaving Flow returns the same board: lanes paint again and a card with no
# epic opens its panel without ever asking for a graph.
xdotool key --clearmodifiers Escape
sleep 0.5
LOOSE_DETAIL_BEFORE=$(flow_detail_requests e2e-ready)
LOOSE_GRAPH_BEFORE=$(epic_graph_requests e2e-flow-epic)
click_card e2e-ready
for _ in $(seq 1 50); do
    [ "$(flow_detail_requests e2e-ready)" -gt "$LOOSE_DETAIL_BEFORE" ] && break
    sleep 0.2
done
[ "$(flow_detail_requests e2e-ready)" -gt "$LOOSE_DETAIL_BEFORE" ] \
    || fail "lanes stopped opening the panel after a Flow round trip"
[ "$(epic_graph_requests e2e-flow-epic)" -eq "$LOOSE_GRAPH_BEFORE" ] \
    || fail "a card with no epic asked for a graph"

import -window "$WID" /output/beads-flow-functional.png
echo 'PASS: a real card click opened the panel and swapped the strip into Flow, a node' \
    'retargeted the panel inside the frozen epic, and lanes stayed usable on return'
