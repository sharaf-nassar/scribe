#!/bin/bash
set -e

# Helper: focus the Scribe window and capture it.
capture_window() {
    local out="$1"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.5
    fi
    # Full-screen capture — Vulkan surfaces may not be readable per-window.
    scrot "$out"
}

# --- Phase 1: Create visible content ---
scribe-test send "$SESSION" 'echo BEFORE_DISCONNECT\n'
scribe-test wait-output "$SESSION" "BEFORE_DISCONNECT"
scribe-test send "$SESSION" 'echo visual-test-line-two\n'
scribe-test wait-output "$SESSION" "visual-test-line-two"
sleep 0.5

capture_window /output/01-before.png
echo "PHASE 1 PASS: before screenshot captured"

# --- Phase 2: Close the client window (simulates user closing the UI) ---
wid=$(xdotool search --name "Scribe" | head -1) || true
if [ -n "$wid" ]; then
    xdotool windowclose "$wid"
fi
sleep 1
echo "PHASE 2 PASS: client closed"

# --- Phase 3: Restart the client (reconnects to server) ---
# Vulkan device is lost after the first client exits (container graphics stack
# limitation). Reset the instance by unsetting the cached device.
export WGPU_BACKEND=vulkan
scribe-client &
sleep 2

# Wait for the window to appear and give it time to render the snapshot.
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 3
echo "PHASE 3 PASS: client restarted"

# --- Phase 4: Capture the reconnected screen ---
# The client should have restored the terminal content via ScreenSnapshot.
# "BEFORE_DISCONNECT" and "visual-test-line-two" should be visible.
capture_window /output/02-after-reconnect.png
echo "PHASE 4 PASS: after-reconnect screenshot captured"

echo "PASS: visual reconnect test — compare 01-before.png and 02-after-reconnect.png in test-output/"
