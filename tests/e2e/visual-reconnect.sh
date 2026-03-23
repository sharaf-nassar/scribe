#!/bin/bash
set -e

# --- Phase 1: Create visible content ---
scribe-test send "$SESSION" 'echo BEFORE_DISCONNECT\n'
scribe-test wait-output "$SESSION" "BEFORE_DISCONNECT"
scribe-test send "$SESSION" 'echo visual-test-line-two\n'
scribe-test wait-output "$SESSION" "visual-test-line-two"
sleep 0.5

# Take "before" screenshot of the actual GUI window.
scrot /output/01-before.png
echo "PHASE 1 PASS: before screenshot captured"

# --- Phase 2: Kill the client (simulates closing the UI) ---
killall scribe-client 2>/dev/null || true
sleep 0.5
echo "PHASE 2 PASS: client killed"

# --- Phase 3: Restart the client (reconnects to server) ---
export WGPU_BACKEND=gl
scribe-client &
sleep 2

# Wait for the window to appear.
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 1
echo "PHASE 3 PASS: client restarted"

# --- Phase 4: Capture the reconnected screen ---
scrot /output/02-after-reconnect.png
echo "PHASE 4 PASS: after-reconnect screenshot captured"

# --- Phase 5: Verify the session is still usable ---
scribe-test send "$SESSION" 'echo AFTER_RECONNECT\n'
scribe-test wait-output "$SESSION" "AFTER_RECONNECT"
scrot /output/03-after-command.png
echo "PHASE 5 PASS: commands work after reconnect"

echo "PASS: visual reconnect test completed — inspect screenshots in test-output/"
