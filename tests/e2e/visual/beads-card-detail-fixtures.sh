#!/bin/bash
# e2e-timeout: 90
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
# @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures]]
set -euo pipefail

CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

fail() {
    echo "FAIL: $1"
    tail -60 "$CLIENT_LOG" 2>/dev/null || true
    exit 1
}

window_id() {
    xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.5
}

first_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys

found = None
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == "client" and message.get("type") == "CreateSession":
            found = message["workspace_id"]
if found:
    print(found)
else:
    raise SystemExit(1)
PY
}

sleep 2
WID=$(window_id)
[ -n "$WID" ] || fail "no Scribe window"
WORKSPACE=$(first_workspace) || fail "no SessionList workspace recorded"
xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":null}"
import -window "$WID" /output/beads-detail-before.png

# Each fixture is already a complete ServerMessage. The runtime workspace id is
# the only substitution. Loading sends a board and withholds the detail reply.
DETAIL_MESSAGES=$(python3 - /tests/fixtures/beads-card-detail.json "$WORKSPACE" <<'PY'
import json, sys

with open(sys.argv[1]) as handle:
    fixtures = json.load(handle)
expected = ["loading", "closed", "blocked", "comment-clamped", "hidden-count"]
names = [fixture["name"] for fixture in fixtures]
if names != expected:
    raise SystemExit(f"detail fixture names {names!r} != {expected!r}")
for fixture in fixtures:
    message = fixture["message"]
    message["workspace_id"] = sys.argv[2]
    print(f'{fixture["name"]}\t{json.dumps(message, separators=(",", ":"))}')
PY
)

DETAIL_DROPS_BEFORE=$(grep -c \
    'server message not wired into the GPUI client.*BeadsIssueDetail' \
    "$CLIENT_LOG" 2>/dev/null || true)
while IFS=$'\t' read -r variant message; do
    [ -n "$variant" ] && [ -n "$message" ] || fail "empty card-detail fixture row"
    inject "$message"
done <<<"$DETAIL_MESSAGES"
DETAIL_DROPS_AFTER=$(grep -c \
    'server message not wired into the GPUI client.*BeadsIssueDetail' \
    "$CLIENT_LOG" 2>/dev/null || true)
[ "$(( DETAIL_DROPS_AFTER - DETAIL_DROPS_BEFORE ))" -eq 4 ] \
    || fail "share-inject decoded $(( DETAIL_DROPS_AFTER - DETAIL_DROPS_BEFORE )) of 4 detail fixtures"
if grep -q 'could not decode an injection' /output/share-tap.log; then
    fail "share-tap rejected a card-detail fixture"
fi

# The loading variant's board is handled today. Hovering its badge must reveal
# the five fixture cards. Detail messages remain intentionally unwired until
# the panel tasks replace the receipt oracle above with panel screenshots.
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.5
import -window "$WID" /output/beads-detail-loading.png
LOADING_DIFF=$(compare -metric AE /output/beads-detail-before.png \
    /output/beads-detail-loading.png null: 2>&1 || true)
LOADING_DIFF=${LOADING_DIFF%%.*}
[ "${LOADING_DIFF:-0}" -ge 10000 ] \
    || fail "loading fixture board changed only ${LOADING_DIFF:-0}px"

echo "PASS: loading board rendered and 4 card-detail variants decoded through share-inject"
