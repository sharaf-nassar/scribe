#!/bin/bash
set -e

# Failure path: socket vanishing mid-session (hard server loss / crash).
#
# Proves the client detects a mid-session server disconnect and tears its IPC
# down instead of hanging on a dead socket, and that it can then reconnect to a
# freshly started server. Contrasts with hot-reload.sh: a hard crash (SIGTERM,
# no fd handoff) does NOT preserve sessions, so recovery is a fresh session
# rather than an adopt. The daemon is the client stand-in; runs against the
# disposable test server only.

# --- Phase 1: Establish a live mid-session baseline ---
scribe-test send "$SESSION" 'echo pre-crash-marker\n'
scribe-test wait-output "$SESSION" "pre-crash-marker"
DEAD_SESSION="$SESSION"
echo "PHASE 1 PASS: live session established before the crash"

# --- Phase 2: Crash the server mid-session (socket vanishes under the client) ---
# `server stop` SIGTERMs the server with no upgrade handoff, so the client's
# IPC connection drops and its PTYs die with the server.
scribe-test server stop
echo "PHASE 2 PASS: server crashed (SIGTERM, no handoff)"

# --- Phase 3: Client detects the loss and tears down (no hang on a dead socket) ---
# The daemon's server-reader loop ends when the connection drops, so it shuts
# down and removes its command socket. Poll until commands to it fail, proving
# the client noticed the loss rather than blocking forever.
DETECTED=0
for _ in $(seq 1 40); do
    if ! scribe-test send "$DEAD_SESSION" 'echo probe\n' 2>/dev/null; then
        DETECTED=1
        break
    fi
    sleep 0.25
done
if [ "$DETECTED" -ne 1 ]; then
    echo "PHASE 3 FAIL: client did not detect the socket loss"
    exit 1
fi
echo "PHASE 3 PASS: client detected socket loss and stopped serving commands"

# --- Phase 4: Reconnect to a freshly started server ---
scribe-test server start
scribe-test daemon start
echo "PHASE 4 PASS: server restarted and client reconnected"

# --- Phase 5: The crashed session is gone; adopting it must fail ---
# A hard crash keeps no PTYs, so the pre-crash session cannot be reattached.
if scribe-test session attach "$DEAD_SESSION" 2>/dev/null; then
    echo "PHASE 5 FAIL: reattached a session that died with the crashed server"
    exit 1
fi
echo "PHASE 5 PASS: crashed session correctly unavailable after restart"

# --- Phase 6: A fresh session on the restarted server works end to end ---
FRESH=$(scribe-test session create)
scribe-test send "$FRESH" 'echo post-crash-recovery\n'
scribe-test wait-output "$FRESH" "post-crash-recovery"
scribe-test session close "$FRESH"
echo "PHASE 6 PASS: fresh session works after crash recovery"

echo "PASS: socket-loss / crash-recovery test completed"
