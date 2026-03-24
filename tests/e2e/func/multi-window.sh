#!/bin/bash
set -e

# Multi-window session isolation test.
#
# Verifies that multiple sessions (simulating separate windows) are
# independent: output in one does not appear in the other, and both
# survive a disconnect/reconnect cycle.

# ── Phase 1: Create a second session ─────────────────────────────
SESSION_A="$SESSION"
SESSION_B=$(scribe-test session create)
echo "PHASE 1 PASS: two sessions created (A=$SESSION_A, B=$SESSION_B)"

# ── Phase 2: Send distinct commands to each session ──────────────
scribe-test send "$SESSION_A" 'echo window-a-marker\n'
scribe-test wait-output "$SESSION_A" "window-a-marker"

scribe-test send "$SESSION_B" 'echo window-b-marker\n'
scribe-test wait-output "$SESSION_B" "window-b-marker"
echo "PHASE 2 PASS: both sessions accept commands"

# ── Phase 3: Verify isolation — A's output is NOT in B ───────────
scribe-test send "$SESSION_A" 'echo isolation-check-a\n'
scribe-test wait-output "$SESSION_A" "isolation-check-a"
scribe-test wait-idle "$SESSION_B" --ms 300

# Dump session B's screen and verify it does NOT contain A's marker
SNAP_B=$(scribe-test snapshot "$SESSION_B" /dev/stdout 2>/dev/null)
if echo "$SNAP_B" | grep -q "isolation-check-a"; then
    echo "PHASE 3 FAIL: session B contains session A's output"
    exit 1
fi
echo "PHASE 3 PASS: sessions are isolated"

# ── Phase 4: Disconnect and reconnect ────────────────────────────
SAVED_A="$SESSION_A"
SAVED_B="$SESSION_B"

scribe-test daemon stop
echo "PHASE 4a: daemon stopped (sessions detached)"

scribe-test daemon start
scribe-test session attach "$SAVED_A"
scribe-test session attach "$SAVED_B"
echo "PHASE 4b: reattached to both sessions"

# ── Phase 5: Verify both sessions survived ───────────────────────
scribe-test send "$SAVED_A" 'echo a-after-reconnect\n'
scribe-test wait-output "$SAVED_A" "a-after-reconnect"

scribe-test send "$SAVED_B" 'echo b-after-reconnect\n'
scribe-test wait-output "$SAVED_B" "b-after-reconnect"
echo "PHASE 5 PASS: both sessions alive after reconnect"

# ── Phase 6: Close one session, verify the other survives ────────
scribe-test session close "$SAVED_B"
scribe-test send "$SAVED_A" 'echo a-still-alive\n'
scribe-test wait-output "$SAVED_A" "a-still-alive"
echo "PHASE 6 PASS: closing one session does not affect the other"

echo "PASS: multi-window session isolation test completed"
