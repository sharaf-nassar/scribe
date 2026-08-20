#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests]]
set -euo pipefail

CONFIG_FILE="$HOME/.config/scribe/config.toml"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

restart_with_read_policy() {
    local mode="$1"
    scribe-test daemon stop >/dev/null 2>&1 || true
    scribe-test server stop >/dev/null 2>&1 || true
    cat >"$CONFIG_FILE" <<TOML
[agent_api]
read_content = "$mode"
TOML
    scribe-test server start
    scribe-test daemon start
}

run_read_inside_pane() {
    local caller="$1" target="$2" output="$3" status_label="$4"
    scribe-test send "$caller" "RUST_LOG=off scribe agent --agent agent-read-e2e read '$target' > '$output' 2> '${output%.json}.stderr'; status=\$?; printf '$status_label:%s\\n' \"\$status\"\n"
}

# Default-safe denial runs from a real Scribe pane and discloses no screen body.
restart_with_read_policy deny
DENIED_CALLER=$(scribe-test session create)
DENIED_SIBLING=$(scribe-test session create)
DENIED_SECRET="agent-read-denied-secret-7f31"
scribe-test send "$DENIED_SIBLING" "printf '$DENIED_SECRET\\n'\n"
scribe-test wait-output "$DENIED_SIBLING" "$DENIED_SECRET"
run_read_inside_pane "$DENIED_CALLER" "$DENIED_SIBLING" /output/agent-read-denied.json agent-read-denied-status
scribe-test wait-output "$DENIED_CALLER" "agent-read-denied-status:1"
grep -q '"ok":false' /output/agent-read-denied.json \
    || fail "denied read did not return a failure envelope"
grep -q '"code":"denied"' /output/agent-read-denied.json \
    || fail "denied read did not return the typed denied code"
if grep -qF "$DENIED_SECRET" /output/agent-read-denied.json \
    || grep -q '"screen"\|"text"' /output/agent-read-denied.json; then
    fail "denied read disclosed terminal content: $(cat /output/agent-read-denied.json)"
fi
echo "PHASE 1 PASS: an in-pane denied read returned no terminal content"

# Opt-in read resolves a sibling by session id and carries identity plus text.
restart_with_read_policy allow
CALLER=$(scribe-test session create)
SIBLING=$(scribe-test session create)
MARKER="agent-read-sibling-visible-4ca8"
scribe-test send "$SIBLING" "printf '$MARKER\\n'\n"
scribe-test wait-output "$SIBLING" "$MARKER"
run_read_inside_pane "$CALLER" "$SIBLING" /output/agent-read-allowed.json agent-read-allowed-status
scribe-test wait-output "$CALLER" "agent-read-allowed-status:0"
grep -q '"ok":true' /output/agent-read-allowed.json \
    || fail "allowed read did not return a success envelope"
grep -q '"type":"read_screen"' /output/agent-read-allowed.json \
    || fail "allowed read returned the wrong payload type"
grep -q "\"session_id\":\"$SIBLING\"" /output/agent-read-allowed.json \
    || fail "allowed read did not identify sibling $SIBLING"
grep -q '"title":' /output/agent-read-allowed.json \
    || fail "allowed read omitted the sibling title"
grep -q '"cwd":' /output/agent-read-allowed.json \
    || fail "allowed read omitted the sibling cwd"
grep -qF "$MARKER" /output/agent-read-allowed.json \
    || fail "allowed read omitted sibling terminal text"
echo "PHASE 2 PASS: an in-pane CLI read returned its sibling's identified screen"

echo "PASS: agent read CLI functional coverage completed"
