#!/bin/bash
# E2E test: workspace split creates independent sessions
#
# Workspace splits are a client-side UI operation (splitting the window
# layout tree) backed by a server-side session creation. This test
# verifies the server can host multiple sessions simultaneously — the
# same thing that happens when the client presses Ctrl+Alt+\ or
# Ctrl+Alt+-.
set -e

# ── Phase 1: initial session works ──────────────────────────────────
scribe-test send "$SESSION" 'echo workspace-A\n'
scribe-test wait-output "$SESSION" "workspace-A"
echo "PHASE 1 PASS: initial session (workspace A) functional"

# ── Phase 2: create a second session (simulates workspace split) ────
SESSION_B=$(scribe-test session create)
echo "PHASE 2 PASS: second session created ($SESSION_B)"

# ── Phase 3: both sessions accept input and produce output ──────────
scribe-test send "$SESSION_B" 'echo workspace-B\n'
scribe-test wait-output "$SESSION_B" "workspace-B"
echo "PHASE 3 PASS: second session (workspace B) functional"

# ── Phase 4: sessions are isolated (output doesn't bleed) ───────────
# Send a unique marker to session A and verify it does NOT appear in B.
scribe-test send "$SESSION" 'echo MARKER_A_ONLY\n'
scribe-test wait-output "$SESSION" "MARKER_A_ONLY"

scribe-test snapshot "$SESSION_B" /output/ws_b_snapshot.json
# The marker should only be in session A's output, not B's.
if grep -q "MARKER_A_ONLY" /output/ws_b_snapshot.json; then
    echo "FAIL: session A output leaked into session B"
    exit 1
fi
echo "PHASE 4 PASS: sessions are isolated"

# ── Phase 5: both sessions survive after concurrent use ─────────────
scribe-test send "$SESSION" 'echo final-A\n'
scribe-test send "$SESSION_B" 'echo final-B\n'
scribe-test wait-output "$SESSION" "final-A"
scribe-test wait-output "$SESSION_B" "final-B"
echo "PHASE 5 PASS: both sessions healthy after concurrent use"

echo "PASS: workspace split test completed"
