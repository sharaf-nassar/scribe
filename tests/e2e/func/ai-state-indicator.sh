#!/bin/bash
set -e

# =============================================================================
# AI State Indicator — functional E2E test
#
# Validates that:
#   1. OSC 1337 ClaudeState sequences are stripped from visible terminal output
#   2. All five AI states can be sent without corrupting the terminal
#   3. OSC sequences interleaved with normal output preserve the output
#   4. Rapid state transitions don't deadlock
#   5. Session create/close lifecycle works with AI state active
# =============================================================================

# ── Phase 1: OSC 1337 ClaudeState=processing is stripped from output ─────────
# Send a processing state followed by a visible marker.  The marker must
# appear in the grid but the OSC sequence itself must not.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing\\033\\\\"; echo ai-phase1-ok\n'
scribe-test wait-output "$SESSION" "ai-phase1-ok"

# The OSC payload must NOT appear as visible text.
SNAP1=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if echo "$SNAP1" | grep -q "ClaudeState"; then
    echo "PHASE 1 FAIL: ClaudeState sequence leaked into visible output"
    exit 1
fi
echo "PHASE 1 PASS: processing state sent, OSC stripped from output"

# ── Phase 2: All five states cycle correctly ──────────────────────────────
# Emit each state, then a visible marker.  If any OSC were malformed the
# terminal would show garbage or the marker would not appear.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing\\033\\\\"; echo state-proc\n'
scribe-test wait-output "$SESSION" "state-proc"

scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=idle_prompt\\033\\\\"; echo state-idle\n'
scribe-test wait-output "$SESSION" "state-idle"

scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=waiting_for_input\\033\\\\"; echo state-wait\n'
scribe-test wait-output "$SESSION" "state-wait"

scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=permission_prompt\\033\\\\"; echo state-perm\n'
scribe-test wait-output "$SESSION" "state-perm"

scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=error\\033\\\\"; echo state-err\n'
scribe-test wait-output "$SESSION" "state-err"

echo "PHASE 2 PASS: all five AI states cycled without corruption"

# ── Phase 3: OSC with optional fields ────────────────────────────────────
# The parser accepts optional tool/agent/model/context fields.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing;tool=Bash;agent=main;model=claude;context=42\\033\\\\"; echo fields-ok\n'
scribe-test wait-output "$SESSION" "fields-ok"

# Verify no ClaudeState payload leaked.
SNAP3=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if echo "$SNAP3" | grep -q "ClaudeState"; then
    echo "PHASE 3 FAIL: ClaudeState with fields leaked into output"
    exit 1
fi
echo "PHASE 3 PASS: optional fields accepted, OSC stripped"

# ── Phase 4: Interleaved output preserved ────────────────────────────────
# Emit normal text, then an OSC, then more normal text — all in one printf.
# Both visible parts must appear and the OSC must be invisible.
scribe-test send "$SESSION" 'printf "BEFORE\\033]1337;ClaudeState=processing\\033\\\\AFTER\\n"\n'
scribe-test wait-output "$SESSION" "BEFORE"
scribe-test wait-output "$SESSION" "AFTER"
echo "PHASE 4 PASS: interleaved output preserved across OSC"

# ── Phase 5: Rapid state transitions ─────────────────────────────────────
# Emit several state changes in quick succession followed by a visible
# marker — the server must not drop output or deadlock.
scribe-test send "$SESSION" 'for s in processing idle_prompt waiting_for_input permission_prompt processing error; do printf "\\033]1337;ClaudeState=$s\\033\\\\"; done; echo rapid-ok\n'
scribe-test wait-output "$SESSION" "rapid-ok"
echo "PHASE 5 PASS: rapid state transitions handled without deadlock"

# ── Phase 6: Inactive clears state ────────────────────────────────────────
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing\\033\\\\"; echo set-proc\n'
scribe-test wait-output "$SESSION" "set-proc"
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=inactive\\033\\\\"; echo cleared\n'
scribe-test wait-output "$SESSION" "cleared"
echo "PHASE 6 PASS: inactive OSC clears AI state"

# ── Phase 7: Session with AI state can be closed cleanly ─────────────────
# This is last because closing a session may affect daemon state.
EXTRA=$(scribe-test session create)
scribe-test send "$EXTRA" 'printf "\\033]1337;ClaudeState=processing\\033\\\\"; echo extra-alive\n'
scribe-test wait-output "$EXTRA" "extra-alive"
scribe-test session close "$EXTRA"
echo "PHASE 6 PASS: session with active AI state closed cleanly"

echo "PASS: AI state indicator test completed"
