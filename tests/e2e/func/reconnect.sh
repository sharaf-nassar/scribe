#!/bin/bash
set -e

# --- Phase 1: Send a command to the initial session ---
scribe-test send "$SESSION" 'echo reconnect-marker\n'
scribe-test wait-output "$SESSION" "reconnect-marker"
echo "PHASE 1 PASS: initial command executed"

# --- Phase 2: Start a background process that survives disconnect ---
scribe-test send "$SESSION" 'sleep 600 &\n'
scribe-test wait-idle "$SESSION" --ms 300

# Remember the session ID for re-attach.
SAVED_SESSION="$SESSION"

# --- Phase 3: Disconnect by stopping the daemon ---
scribe-test daemon stop
echo "PHASE 3 PASS: daemon stopped (session detached)"

# --- Phase 4: Start a new daemon (new IPC connection to server) ---
scribe-test daemon start

# --- Phase 5: Reattach to the saved session ---
scribe-test session attach "$SAVED_SESSION"
echo "PHASE 5 PASS: reattached to session $SAVED_SESSION"

# --- Phase 6: Verify the session is alive — send a new command ---
scribe-test send "$SAVED_SESSION" 'echo after-reconnect\n'
scribe-test wait-output "$SAVED_SESSION" "after-reconnect"
echo "PHASE 6 PASS: command executed after reconnect"

# --- Phase 7: Verify the background process survived ---
scribe-test send "$SAVED_SESSION" 'jobs\n'
scribe-test wait-output "$SAVED_SESSION" "sleep 600"
echo "PHASE 7 PASS: background process survived disconnect"

# Clean up the background sleep
scribe-test send "$SAVED_SESSION" 'kill %1 2>/dev/null; true\n'
scribe-test wait-idle "$SAVED_SESSION" --ms 300

echo "PASS: reconnect test completed"
