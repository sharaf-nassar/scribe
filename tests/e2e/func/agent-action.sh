#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual-agent-action)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    tail -40 "${SCRIBE_CLIENT_LOG:-/output/client.log}" 2>/dev/null >&2 || true
    exit 1
}

command -v scribe >/dev/null 2>&1 || fail "scribe CLI is absent from the visual harness"

# The default visual rig has one daemon window and one real GPUI window. Name
# the latter explicitly so correlated completion cannot route to the headless
# harness participant.
DAEMON_WINDOW=$(scribe-test daemon window-id)
WINDOWS=$(RUST_LOG=off scribe windows)
TARGET_WINDOWS=$(printf '%s\n' "$WINDOWS" | awk -v daemon="$DAEMON_WINDOW" \
    '$1 != daemon && $3 == "connected" { print $1 }')
[ "$(printf '%s\n' "$TARGET_WINDOWS" | awk 'NF { n++ } END { print n + 0 }')" -eq 1 ] \
    || fail "expected one GPUI window beside daemon $DAEMON_WINDOW: $WINDOWS"
TARGET_WINDOW=$(printf '%s\n' "$TARGET_WINDOWS" | awk 'NF { print; exit }')

set +e
SCRIBE_SESSION_ID="$SESSION" RUST_LOG=off \
    scribe agent --agent agent-action-e2e action --window "$TARGET_WINDOW" new-tab \
    >/output/agent-action.json 2>/output/agent-action.stderr
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "new-tab action exited $STATUS: $(cat /output/agent-action.json)"
grep -q '"ok":true' /output/agent-action.json \
    || fail "new-tab action did not return a success envelope"
grep -q '"type":"dispatch_action"' /output/agent-action.json \
    || fail "new-tab action returned the wrong payload type"
grep -q '"outcome":"completed"' /output/agent-action.json \
    || fail "new-tab action was not completed"

CREATED_SESSION_ID=$(sed -n 's/.*"created_session_id":"\([^"]*\)".*/\1/p' /output/agent-action.json)
case "$CREATED_SESSION_ID" in
    ????????-????-????-????-????????????) ;;
    *) fail "new-tab action returned no created session id: $(cat /output/agent-action.json)" ;;
esac
echo "PHASE 1 PASS: correlated new-tab returned created session $CREATED_SESSION_ID"

SCRIBE_SESSION_ID="$SESSION" RUST_LOG=off \
    scribe agent --agent agent-action-e2e world \
    >/output/agent-action-world.json 2>/output/agent-action-world.stderr
grep -q "\"session_id\":\"$CREATED_SESSION_ID\"" /output/agent-action-world.json \
    || fail "created session $CREATED_SESSION_ID is absent from the real server world"
echo "PHASE 2 PASS: the returned id names the tab the GPUI client created"

echo "PASS: agent action CLI functional coverage completed"
