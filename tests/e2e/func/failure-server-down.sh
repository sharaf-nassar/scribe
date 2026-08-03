#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Failure paths: server-down at launch + adoption failure.
#
# Proves the client fails loudly (never silently or with a hang) when the
# server is unavailable at launch, and when it is asked to adopt a session
# that does not exist, then recovers cleanly once the server is back. The
# daemon is the client stand-in. Runs against the disposable test server only.

# --- Phase 1: Tear everything down to reach a server-down launch state ---
scribe-test daemon stop
scribe-test server stop
echo "PHASE 1 PASS: server and client stopped (server-down state reached)"

# --- Phase 2: Client launch with the server down must FAIL, not hang ---
# `daemon start` spawns the client daemon, which immediately tries to connect
# to the (absent) server socket; the connect fails, the daemon exits, and its
# own socket never appears, so `daemon start` returns non-zero within its
# bounded wait rather than blocking forever.
if scribe-test daemon start 2>/dev/null; then
    echo "PHASE 2 FAIL: client launch unexpectedly succeeded with server down"
    scribe-test daemon stop >/dev/null 2>&1 || true
    exit 1
fi
echo "PHASE 2 PASS: client launch failed cleanly with server down"

# --- Phase 3: Recover — bring the server back, client attaches normally ---
scribe-test server start
scribe-test daemon start
RECOVERED=$(scribe-test session create)
scribe-test send "$RECOVERED" 'echo recovered-after-server-down\n'
scribe-test wait-output "$RECOVERED" "recovered-after-server-down"
echo "PHASE 3 PASS: client recovered and created a working session"

# --- Phase 4: Adoption failure — attaching a nonexistent session errors ---
# Restart the client so no prior SessionCreated is cached, then attempt to
# adopt a session id that was never created. The server denies the adoption,
# so the attach returns non-zero (error) without crashing the client.
scribe-test daemon stop
scribe-test daemon start
BOGUS="00000000-0000-4000-8000-000000000000"
if scribe-test session attach "$BOGUS" 2>/dev/null; then
    echo "PHASE 4 FAIL: adopting a nonexistent session unexpectedly succeeded"
    exit 1
fi
echo "PHASE 4 PASS: adoption of a nonexistent session failed cleanly"

# --- Phase 5: Client survived the failed adoption and still works ---
STILL=$(scribe-test session create)
scribe-test send "$STILL" 'echo survived-adoption-failure\n'
scribe-test wait-output "$STILL" "survived-adoption-failure"
scribe-test session close "$STILL"
echo "PHASE 5 PASS: client survived adoption failure and stayed usable"

echo "PASS: failure server-down + adoption test completed"
