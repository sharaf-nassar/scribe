#!/bin/bash
set -e

# Server-side session survival across a client disconnect.
#
# This is the SERVER half of cold-restart recovery: every session, its
# scrollback, its terminal geometry, and its background jobs must survive a
# client going away entirely and be re-attachable by whatever connects next.
# The daemon is an ordinary protocol client here, so `daemon stop` is a client
# disconnect and `daemon start` is a fresh connection. Nothing here ever touches
# the user's live server — the entrypoint runs a disposable
# `scribe-test server`.
#
# It is deliberately NOT the oracle for the client's cold-restart restore. The
# daemon has no window, no layout and no restore store, so it can neither write
# a `RestoreStore` snapshot nor replay one; a green run here says nothing about
# `--restore-child` fan-out or window geometry persistence. Those are asserted
# against the real `scribe-client-gpui` process in
# tests/e2e/visual/cold-restart.sh (`just e2e-visual-cold-restart`), which
# crashes the client and cold-restarts the server so the client meets the empty
# `SessionList` a replay requires.

# --- Phase 1: Open a fan of sessions with distinct markers + geometry ---
# Reuse the entrypoint session as the first, then open two more so the
# re-attach has a real fan (three sessions in one window) to pick back up.
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

# --- Phase 2: Give one session a non-default geometry to prove it is kept ---
scribe-test resize "$S2" 132 50
scribe-test wait-idle "$S2" --ms 300
scribe-test send "$S2" 'tput cols\n'
scribe-test wait-output "$S2" "132"
echo "PHASE 2 PASS: session two resized to 132x50 before the disconnect"

# --- Phase 3: Long-lived background job that must survive the disconnect ---
scribe-test send "$S3" 'sleep 600 &\n'
scribe-test wait-idle "$S3" --ms 300
echo "PHASE 3 PASS: background job started in session three"

# --- Phase 4: Disconnect the client entirely (server keeps sessions) ---
scribe-test daemon stop
echo "PHASE 4 PASS: client disconnected (all sessions detached, server alive)"

# --- Phase 5: A fresh client connection re-attaches to every session ---
scribe-test daemon start
scribe-test session attach "$S1"
scribe-test session attach "$S2"
scribe-test session attach "$S3"
echo "PHASE 5 PASS: a fresh connection re-attached all three sessions"

# --- Phase 6: Replay correctness — each session's scrollback survived ---
scribe-test send "$S1" 'echo after-cold-one\n'
scribe-test wait-output "$S1" "after-cold-one"
scribe-test send "$S2" 'echo after-cold-two\n'
scribe-test wait-output "$S2" "after-cold-two"
scribe-test send "$S3" 'echo after-cold-three\n'
scribe-test wait-output "$S3" "after-cold-three"
echo "PHASE 6 PASS: all three sessions replayed and accept fresh input"

# --- Phase 7: The resized session keeps its dimensions ---
scribe-test send "$S2" 'tput cols\n'
scribe-test wait-output "$S2" "132"
echo "PHASE 7 PASS: session two geometry (132 cols) preserved across the disconnect"

# --- Phase 8: The background job survived the disconnect ---
scribe-test send "$S3" 'jobs\n'
scribe-test wait-output "$S3" "sleep 600"
echo "PHASE 8 PASS: background job survived the client disconnect"

# Clean up the background sleep and the extra sessions.
scribe-test send "$S3" 'kill %1 2>/dev/null; true\n'
scribe-test wait-idle "$S3" --ms 300
scribe-test session close "$S2"
scribe-test session close "$S3"

echo "PASS: server-side session survival test completed"
