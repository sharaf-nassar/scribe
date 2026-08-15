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
        bd create 'Deferred detail fixture' --id e2e-deferred --type task --priority 4 \
            --defer '2030-03-01' >/dev/null
        bd create 'Real board refresh' --id e2e-ready --type task --priority 1 >/dev/null
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
printf '%s\n' "$BOARD" | grep -Fq '"ready_total":2' \
    || fail "Ready total included the epic record: $BOARD"
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

if [ -z "${DISPLAY:-}" ]; then
    echo 'PASS: real bd refreshed the board and returned complete deterministic detail fixtures'
    exit 0
fi

WID=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
[ -n "$WID" ] || fail "real-bd detail run found no Scribe window"
xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID"
import -window "$WID" /output/beads-real-before.png

detail_request_count() {
    python3 - "$RECORD" <<'PY'
import json, sys
count = 0
for line in open(sys.argv[1]):
    try: row = json.loads(line)
    except ValueError: continue
    message = row.get("message", {})
    if (row.get("dir") == "client"
            and message.get("type") == "RequestBeadsIssueDetail"
            and message.get("issue_id") == "e2e-detail"):
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

# Hover the rooted workspace's bead and wait for the real board to paint.
xdotool mousemove --sync --window "$WID" 91 17
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
# drag arm. This must remain a card click and request its complete detail.
REQUESTS_BEFORE=$(detail_request_count)
xdotool mousemove --sync --window "$WID" 700 84
xdotool mousedown 1
xdotool mouseup 1
for _ in $(seq 1 30); do
    [ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 1))" ] && break
    sleep 0.2
done
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 1))" ] \
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
sleep 0.2
import -window "$WID" /output/beads-real-detail.png
DETAIL_DIFF=$(compare -metric AE /output/beads-real-board.png \
    /output/beads-real-detail.png null: 2>&1 || true)
DETAIL_DIFF=${DETAIL_DIFF%%.*}
[ "${DETAIL_DIFF:-0}" -ge 20000 ] || fail "real detail panel changed only ${DETAIL_DIFF:-0}px"

# The painted panel's identity target is the final semantic check: it copies
# the same full id read from bd show and carried on the matched wire response.
xdotool mousemove --sync --window "$WID" 450 284
xdotool click 1
sleep 0.2
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "e2e-detail" ] || fail "painted panel copied '$COPIED' instead of e2e-detail"

# The real editor owns keyboard input while armed. The transparent wire tap is
# the byte-level pane oracle: cancelled title text must create neither KeyInput
# frames nor a write request.
WRITES_BEFORE=$(write_request_count)
KEYS_BEFORE=$(key_input_count)
xdotool mousemove --sync --window "$WID" 560 258
xdotool click 1
sleep 0.2
xdotool type --clearmodifiers --delay 5 -- 'EDITOR_KEYS_STAY_LOCAL'
sleep 0.2
KEYS_AFTER=$(key_input_count)
[ "$KEYS_AFTER" -eq "$KEYS_BEFORE" ] \
    || fail "armed Beads editor forwarded keystrokes to the pane program"
[ "$(write_request_count)" -eq "$WRITES_BEFORE" ] \
    || fail "typing into the armed editor committed before Enter"
xdotool key --clearmodifiers Escape
sleep 0.2
[ "$(write_request_count)" -eq "$WRITES_BEFORE" ] \
    || fail "cancelling the armed editor queued a write"

# Install the deterministic bd fault shim after the capability handshake. It
# delegates every read and targets only writes for this fixture.
mv /usr/local/bin/bd /usr/local/bin/bd-real
ln -s /tests/fixtures/bd-write-fault.sh /usr/local/bin/bd
rm -f /tmp/scribe-beads-write-fault-mode

xdotool mousemove --sync --window "$WID" 900 600
sleep 0.2
import -window "$WID" /output/beads-write-last-good.png

printf '%s\n' nonzero:e2e-detail >/tmp/scribe-beads-write-fault-mode
xdotool mousemove --sync --window "$WID" 560 258
xdotool click 1
xdotool type --clearmodifiers --delay 5 -- 'must not persist'
xdotool key --clearmodifiers Return
wait_for_write_failure 'forced nonzero write' 50 \
    || fail "GPUI nonzero write produced no typed Failed result"
rm -f /tmp/scribe-beads-write-fault-mode
NONZERO_SHOW=$(cd "$PROJECT" && bd show e2e-detail --json)
printf '%s\n' "$NONZERO_SHOW" | grep -Fq '"title": "Complete card detail"' \
    || fail "GPUI nonzero write replaced last-good detail: $NONZERO_SHOW"
xdotool mousemove --sync --window "$WID" 900 600
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
TIMEOUT_DETAIL_BEFORE=$(detail_request_count)
TIMEOUT_BOARD_BEFORE=$(board_request_count)
printf '%s\n' timeout:e2e-detail >/tmp/scribe-beads-write-fault-mode
xdotool mousemove --sync --window "$WID" 560 258
xdotool click 1
xdotool type --clearmodifiers --delay 5 -- 'must not time in'
xdotool key --clearmodifiers Return
wait_for_write_failure 'bd issue write timed out' 100 \
    || fail "GPUI timeout write produced no typed Failed result"
rm -f /tmp/scribe-beads-write-fault-mode
for _ in $(seq 1 50); do
    [ "$(detail_request_count)" -gt "$TIMEOUT_DETAIL_BEFORE" ] \
        && [ "$(board_request_count)" -gt "$TIMEOUT_BOARD_BEFORE" ] \
        && break
    sleep 0.2
done
[ "$(detail_request_count)" -gt "$TIMEOUT_DETAIL_BEFORE" ] \
    || fail "timeout did not request authoritative detail"
[ "$(board_request_count)" -gt "$TIMEOUT_BOARD_BEFORE" ] \
    || fail "timeout did not request an authoritative board"
TIMEOUT_SHOW=$(cd "$PROJECT" && bd show e2e-detail --json)
printf '%s\n' "$TIMEOUT_SHOW" | grep -Fq '"title": "Complete card detail"' \
    || fail "GPUI timeout write replaced last-good detail: $TIMEOUT_SHOW"
printf '%s\n' "$TIMEOUT_SHOW" >/output/beads-write-gpui-final-show.json
xdotool mousemove --sync --window "$WID" 900 600
sleep 0.2
import -window "$WID" /output/beads-write-timeout-notice.png
TIMEOUT_NOTICE_DIFF=$(compare -metric AE /output/beads-write-last-good.png \
    /output/beads-write-timeout-notice.png null: 2>&1 || true)
TIMEOUT_NOTICE_DIFF=${TIMEOUT_NOTICE_DIFF%%.*}
[ "${TIMEOUT_NOTICE_DIFF:-0}" -ge 500 ] \
    || fail "timeout write painted no failure notice (${TIMEOUT_NOTICE_DIFF:-0}px)"

echo 'PASS: real bd detail persisted, editor input stayed local, and write failures retained last-good state with notices'
