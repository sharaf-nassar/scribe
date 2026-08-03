#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#CLI Smoke E2E]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# The headless daemon is the only persistent local client. A transient
# ListWindows request must report it without allocating a second CLI window.
DAEMON_WINDOW_ID=$(scribe-test daemon window-id)
WINDOWS_OUTPUT=$(RUST_LOG=off scribe windows)
WINDOW_ROWS=$(printf '%s\n' "$WINDOWS_OUTPUT" | awk 'NF { count++ } END { print count + 0 }')
MATCHING_ROWS=$(printf '%s\n' "$WINDOWS_OUTPUT" | awk -v id="$DAEMON_WINDOW_ID" '
    $1 == id && $2 ~ /^[0-9]+$/ && $3 == "connected" && NF == 3 { count++ }
    END { print count + 0 }
')

[ "$WINDOW_ROWS" -eq 1 ] \
    || fail "scribe windows created or reported an extra window: $WINDOWS_OUTPUT"
[ "$MATCHING_ROWS" -eq 1 ] \
    || fail "scribe windows did not name connected daemon window $DAEMON_WINDOW_ID: $WINDOWS_OUTPUT"
echo "PHASE 1 PASS: windows reports only the connected daemon window"

# Dispatch through the real CLI and observe the server-routed RunAction on the
# target daemon. Clearing first prevents an earlier action from satisfying it.
scribe-test daemon clear-action
[ "$(scribe-test daemon last-action)" = "none" ] \
    || fail "action oracle was not clear before dispatch"
RUST_LOG=off scribe action --window "$DAEMON_WINDOW_ID" new-tab

LAST_ACTION=none
for _ in {1..50}; do
    LAST_ACTION=$(scribe-test daemon last-action)
    [ "$LAST_ACTION" = "NewTab" ] && break
    sleep 0.1
done
[ "$LAST_ACTION" = "NewTab" ] \
    || fail "RunAction did not reach daemon; last action was $LAST_ACTION"
echo "PHASE 2 PASS: action routed to daemon as RunAction NewTab"

# Read-only profile commands must work headlessly and must not create or alter
# the profile store as a side effect.
PROFILE_STORE="$HOME/.config/scribe/profiles.toml"
profile_store_state() {
    if [ -e "$PROFILE_STORE" ]; then
        sha256sum "$PROFILE_STORE"
    else
        printf 'absent\n'
    fi
}

PROFILE_STATE_BEFORE=$(profile_store_state)
ACTIVE_PROFILE=$(RUST_LOG=off scribe profile active)
PROFILE_LIST=$(RUST_LOG=off scribe profile list)
PROFILE_STATE_AFTER=$(profile_store_state)

[ -n "$ACTIVE_PROFILE" ] || fail "scribe profile active returned no profile"
ACTIVE_ROWS=$(printf '%s\n' "$PROFILE_LIST" | awk -v active="$ACTIVE_PROFILE" '
    $1 == "*" {
        $1 = ""
        sub(/^ /, "")
        if ($0 == active) count++
    }
    END { print count + 0 }
')
[ "$ACTIVE_ROWS" -eq 1 ] \
    || fail "profile list did not mark active profile $ACTIVE_PROFILE: $PROFILE_LIST"
[ "$PROFILE_STATE_BEFORE" = "$PROFILE_STATE_AFTER" ] \
    || fail "read-only profile commands mutated $PROFILE_STORE"
echo "PHASE 3 PASS: active/list profile output is valid and read-only"

# End in the absent-server state. The entrypoint cleanup may repeat these
# stops, but only this disposable container's daemon and server are targeted.
scribe-test daemon stop
scribe-test server stop

set +e
SERVER_ERROR=$(RUST_LOG=off scribe windows 2>&1)
SERVER_STATUS=$?
set -e

[ "$SERVER_STATUS" -ne 0 ] \
    || fail "scribe windows unexpectedly succeeded without the server"
printf '%s\n' "$SERVER_ERROR" | grep -q 'Error:' \
    || fail "server-down output lacked an error label: $SERVER_ERROR"
printf '%s\n' "$SERVER_ERROR" | grep -Eiq 'No such file or directory|Connection refused|socket' \
    || fail "server-down output lacked useful socket failure text: $SERVER_ERROR"
echo "PHASE 4 PASS: server absence returns nonzero with socket error text"

echo "PASS: scribe CLI headless smoke test completed"
