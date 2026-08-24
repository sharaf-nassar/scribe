#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Multiple server sessions stay isolated. Real multi-window restore and
# reconnect behavior belongs to the GPUI lifecycle suites.
SESSION_A="$SESSION"
SESSION_B=$(scribe-test session create)

echo "PHASE 1 PASS: two sessions created (A=$SESSION_A, B=$SESSION_B)"

scribe-test send "$SESSION_A" 'echo window-a-marker\n'
scribe-test wait-output "$SESSION_A" "window-a-marker"
scribe-test send "$SESSION_B" 'echo window-b-marker\n'
scribe-test wait-output "$SESSION_B" "window-b-marker"
echo "PHASE 2 PASS: both sessions accept commands"

scribe-test send "$SESSION_A" 'echo isolation-check-a\n'
scribe-test wait-output "$SESSION_A" "isolation-check-a"
scribe-test wait-idle "$SESSION_B" --ms 300

SNAP_B=$(scribe-test snapshot "$SESSION_B" /dev/stdout 2>/dev/null)
if echo "$SNAP_B" | grep -q "isolation-check-a"; then
    echo "PHASE 3 FAIL: session B contains session A's output"
    exit 1
fi

echo "PASS: concurrent server sessions remain isolated"
