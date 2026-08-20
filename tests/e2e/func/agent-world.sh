#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests]]
set -euo pipefail

# shellcheck source=tests/e2e/func/agent-common.bash
. /tests/func/agent-common.bash

restart_with_agent_config '[agent_api]
read_metadata = "allow"'

CALLER=$(scribe-test session create)
SIBLING=$(scribe-test session create)

run_inside_caller() {
    local command="$1" output="$2" label="$3"
    send_agent_cli "$CALLER" agent-world-e2e "$command" "$output" "$label"
    scribe-test wait-output "$CALLER" "$label:0"
}

run_inside_caller world /output/agent-world.json agent-world-status
run_inside_caller siblings /output/agent-siblings.json agent-siblings-status
run_inside_caller capabilities /output/agent-capabilities.json agent-capabilities-status

for file in /output/agent-world.json /output/agent-siblings.json /output/agent-capabilities.json; do
    grep -q '"ok":true' "$file" || fail "$file did not contain a success envelope"
done

WORLD_CALLERS=$(grep -o '"is_caller":true' /output/agent-world.json | wc -l || true)
[ "$WORLD_CALLERS" -eq 1 ] \
    || fail "world returned $WORLD_CALLERS caller rows instead of exactly one"
grep -Eq "\"session_id\":\"$CALLER\"[^}]*\"is_caller\":true" /output/agent-world.json \
    || fail "world did not mark caller session $CALLER"
grep -q "\"session_id\":\"$SIBLING\"" /output/agent-world.json \
    || fail "world omitted sibling session $SIBLING"
echo "PHASE 1 PASS: world marked exactly one caller and listed its sibling"

SIBLING_CALLERS=$(grep -o '"is_caller":true' /output/agent-siblings.json | wc -l || true)
[ "$SIBLING_CALLERS" -eq 1 ] \
    || fail "siblings returned $SIBLING_CALLERS caller rows instead of exactly one"
grep -q '"type":"siblings"' /output/agent-siblings.json \
    || fail "siblings returned the wrong payload type"
grep -q "\"session_id\":\"$CALLER\"" /output/agent-siblings.json \
    || fail "siblings omitted caller session $CALLER"
grep -q "\"session_id\":\"$SIBLING\"" /output/agent-siblings.json \
    || fail "siblings omitted sibling session $SIBLING"
echo "PHASE 2 PASS: siblings resolved the caller's window with no id plumbing"

grep -q '"type":"capabilities"' /output/agent-capabilities.json \
    || fail "capabilities returned the wrong payload type"
grep -q '"version":1' /output/agent-capabilities.json \
    || fail "capabilities omitted surface version 1"
grep -q '"capability":"read_metadata","mode":"allow"' /output/agent-capabilities.json \
    || fail "capabilities did not report the live read_metadata policy"
echo "PHASE 3 PASS: capabilities reported the build version and live policy"

echo "PASS: agent world, siblings, and capabilities CLI coverage completed"
