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

# --- Phase 5b: The attach replay is inflated, applied, and ordered ---
# The daemon rebuilds the session's screen from the `SessionReplay` frame the
# server sends after `SessionCreated`, so pre-detach content is observable on
# the replay path itself rather than only through `RequestSnapshot`.
scribe-test replay status "$SAVED_SESSION" --min-frames 1 --timeout 5000
scribe-test replay screen "$SAVED_SESSION" | grep -q "reconnect-marker"
echo "PHASE 5b PASS: replay applied and carries pre-detach content"

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

# --- Phase 8: The replayed view still matches the server's screen ---
# The view holds the replay plus every output byte that followed it, so this
# fails if the attach lost or duplicated output between the server's snapshot
# and its sink install — a gap `RequestSnapshot` alone can never show.
scribe-test replay assert-matches "$SAVED_SESSION"
echo "PHASE 8 PASS: replayed view matches the server screen"

echo "PASS: reconnect test completed"
