#!/bin/bash
set -e

# --- Phase 1: Send a command to the initial session ---
scribe-test send "$SESSION" 'echo hot-reload-marker\n'
scribe-test wait-output "$SESSION" "hot-reload-marker"
echo "PHASE 1 PASS: initial command executed"

# --- Phase 2: Start a background process that should survive the upgrade ---
scribe-test send "$SESSION" 'sleep 600 &\n'
scribe-test wait-idle "$SESSION" --ms 300

# --- Phase 3: Take a pre-upgrade snapshot for comparison ---
scribe-test snapshot "$SESSION" /output/pre-upgrade.json
echo "PHASE 3 PASS: pre-upgrade snapshot captured"

# Remember the session ID.
SAVED_SESSION="$SESSION"

# --- Phase 4: Trigger hot-reload (new server with --upgrade) ---
# The daemon is still connected to the OLD server. We need to:
# 1. Stop the daemon (disconnect from old server)
# 2. Upgrade the server (old -> new via fd handoff)
# 3. Start a new daemon (connects to the new server)
# 4. Reattach to the saved session
scribe-test daemon stop
scribe-test server upgrade
echo "PHASE 4 PASS: server hot-reload completed"

# --- Phase 5: Reconnect daemon and reattach to the session ---
scribe-test daemon start
scribe-test session attach "$SAVED_SESSION"
echo "PHASE 5 PASS: reattached to session $SAVED_SESSION after upgrade"

# --- Phase 6: Verify the session is alive — send a new command ---
scribe-test send "$SAVED_SESSION" 'echo after-upgrade\n'
scribe-test wait-output "$SAVED_SESSION" "after-upgrade"
echo "PHASE 6 PASS: command executed after hot-reload"

# --- Phase 7: Verify the background process survived ---
scribe-test send "$SAVED_SESSION" 'jobs\n'
scribe-test wait-output "$SAVED_SESSION" "sleep 600"
echo "PHASE 7 PASS: background process survived hot-reload"

# --- Phase 8: Verify screen content was preserved (not blank) ---
# The pre-upgrade output should still be visible in the current snapshot.
scribe-test snapshot "$SAVED_SESSION" /output/post-upgrade.json
# Extract cell characters from JSON (each cell is {"c":"X",...}) into a single
# string and check for the marker. The JSON is cell-by-cell, not a flat string.
CELLS=$(grep -oP '"c": "."' /output/post-upgrade.json | cut -d'"' -f4 | tr -d '\n')
if echo "$CELLS" | grep -qF "hot-reload-marker"; then
    echo "PHASE 8 PASS: screen content preserved after hot-reload"
else
    echo "PHASE 8 FAIL: screen content lost after hot-reload"
    echo "  (first 200 chars of cell content: ${CELLS:0:200})"
    exit 1
fi

# Clean up the background sleep
scribe-test send "$SAVED_SESSION" 'kill %1 2>/dev/null; true\n'
scribe-test wait-idle "$SAVED_SESSION" --ms 300

echo "PASS: hot-reload test completed"
