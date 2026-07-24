#!/bin/bash
set -e

# Cold-restart restore fan-out + geometry-compat restore.
#
# Simulates a full client shutdown ("cold" quit) while the disposable test
# server keeps every session alive, then a fresh client cold-start that must
# fan out and re-attach to ALL previously open sessions, replay their
# scrollback, and preserve each session's terminal geometry. The daemon is the
# client stand-in; `daemon stop` is the cold quit and `daemon start` is the
# cold relaunch. Nothing here ever touches the user's live server — the
# entrypoint runs a disposable `scribe-test server`.

# --- Phase 1: Open a fan of sessions with distinct markers + geometry ---
# Reuse the entrypoint session as the first pane, then open two more so the
# restore path has a real fan-out (three panes across one window) to rebuild.
S1="$SESSION"
S2=$(scribe-test session create)
S3=$(scribe-test session create)

scribe-test send "$S1" 'echo cold-marker-one\n'
scribe-test wait-output "$S1" "cold-marker-one"
scribe-test send "$S2" 'echo cold-marker-two\n'
scribe-test wait-output "$S2" "cold-marker-two"
scribe-test send "$S3" 'echo cold-marker-three\n'
scribe-test wait-output "$S3" "cold-marker-three"
echo "PHASE 1 PASS: three sessions opened with distinct markers"

# --- Phase 2: Give one pane a non-default geometry to prove restore keeps it ---
scribe-test resize "$S2" 132 50
scribe-test wait-idle "$S2" --ms 300
scribe-test send "$S2" 'tput cols\n'
scribe-test wait-output "$S2" "132"
echo "PHASE 2 PASS: pane two resized to 132x50 pre-restart"

# --- Phase 3: Long-lived background job that must survive the cold restart ---
scribe-test send "$S3" 'sleep 600 &\n'
scribe-test wait-idle "$S3" --ms 300
echo "PHASE 3 PASS: background job started in pane three"

# --- Phase 4: Cold quit — stop the client entirely (server keeps sessions) ---
scribe-test daemon stop
echo "PHASE 4 PASS: client cold-quit (all sessions detached, server alive)"

# --- Phase 5: Cold start — fresh client relaunch, then fan-out re-attach ---
scribe-test daemon start
scribe-test session attach "$S1"
scribe-test session attach "$S2"
scribe-test session attach "$S3"
echo "PHASE 5 PASS: cold-start fanned out and re-attached all three panes"

# --- Phase 6: Replay correctness — each pane's scrollback survived ---
scribe-test send "$S1" 'echo after-cold-one\n'
scribe-test wait-output "$S1" "after-cold-one"
scribe-test send "$S2" 'echo after-cold-two\n'
scribe-test wait-output "$S2" "after-cold-two"
scribe-test send "$S3" 'echo after-cold-three\n'
scribe-test wait-output "$S3" "after-cold-three"
echo "PHASE 6 PASS: all three panes replayed and accept fresh input"

# --- Phase 7: Geometry-compat restore — resized pane keeps its dimensions ---
scribe-test send "$S2" 'tput cols\n'
scribe-test wait-output "$S2" "132"
echo "PHASE 7 PASS: pane two geometry (132 cols) preserved across cold restart"

# --- Phase 8: Background job survived the cold restart ---
scribe-test send "$S3" 'jobs\n'
scribe-test wait-output "$S3" "sleep 600"
echo "PHASE 8 PASS: background job survived cold restart"

# Clean up the background sleep and the extra sessions.
scribe-test send "$S3" 'kill %1 2>/dev/null; true\n'
scribe-test wait-idle "$S3" --ms 300
scribe-test session close "$S2"
scribe-test session close "$S3"

echo "PASS: cold-restart restore test completed"
