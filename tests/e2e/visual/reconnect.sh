#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
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
# The GPUI client renders through blade/Vulkan on the lavapipe software ICD
# pinned by the image (VK_ICD_FILENAMES), so relaunch needs no GPU reset.
#
# The relaunched client MUST get its own log file and MUST be killed before this
# script exits. A client that inherits the script's stdout and outlives it keeps
# the harness's output plumbing open long after "PASS" is printed, which used to
# wedge the container for the rest of its life: TEST_TIMEOUT governs the test
# process, not whatever that process leaves running.
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
scribe-client >>"$CLIENT_LOG" 2>&1 &
RELAUNCHED_CLIENT_PID=$!
trap 'kill "${RELAUNCHED_CLIENT_PID:-0}" 2>/dev/null || true' EXIT
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

# Tear the relaunched client down and wait for it to actually go, the same way
# every other relaunching visual script ends. Leaving it running is what made
# this test pass and then hang its container.
# The check is on this pid, not on `pgrep scribe-client`: the client the
# entrypoint launched survives phase 2's `windowclose` and is the entrypoint's
# own APP_PID to reap.
kill "$RELAUNCHED_CLIENT_PID" 2>/dev/null || true
for _ in $(seq 1 40); do
    kill -0 "$RELAUNCHED_CLIENT_PID" 2>/dev/null || break
    sleep 0.25
done
if kill -0 "$RELAUNCHED_CLIENT_PID" 2>/dev/null; then
    echo "WARNING: relaunched client ignored SIGTERM; killing it" >&2
    kill -9 "$RELAUNCHED_CLIENT_PID" 2>/dev/null || true
fi

echo "PASS: visual reconnect test — compare 01-before.png and 02-after-reconnect.png in test-output/"
