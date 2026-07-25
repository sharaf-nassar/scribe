#!/bin/bash
set -e

# =============================================================================
# AI State Indicator — functional E2E test
#
# Validates that:
#   1. Hook-channel state events reach the client without disturbing the
#      terminal grid
#   2. All five AI states can be sent without corrupting the terminal
#   3. Optional metadata (context %) reaches the client AI chrome
#   4. Legacy OSC 1337 payloads stay invisible and preserve surrounding output
#   5. Rapid state transitions don't deadlock
#   6. An explicit clear drops the AI chrome
#   7. Session create/close lifecycle works with AI state active
#
# AI state travels over the hook channel (spec 003, FR-020..FR-022), not OSC
# 1337, so the state phases drive `scribe-hook-helper` inside the session shell.
# =============================================================================

HELPER=scribe-hook-helper

# Fire hook events, then park the shell in `read` so it prints no new prompt:
# a prompt (OSC 133;A) means the AI tool exited and the server clears the live
# AI state. Releasing the parked shell is how each phase resets.
hold_ai_state() {
    scribe-test send "$SESSION" "$*; read -r\n"
}

release_ai_state() {
    scribe-test send "$SESSION" '\n'
    sleep 0.3
}

# Poll the AI chrome until `$1` appears, leaving the last reading in $CHROME.
chrome_wait() {
    local needle="$1" attempt=0
    while [ "$attempt" -lt 80 ]; do
        CHROME=$(scribe-test ai-chrome "$SESSION" 2>/dev/null || true)
        if printf '%s\n' "$CHROME" | grep -q -- "$needle"; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    return 1
}

# ── Phase 1: a processing state does not disturb the terminal ────────────────
scribe-test send "$SESSION" "$HELPER --provider=claude_code --event=state_changed --state=processing; echo ai-phase1-ok\n"
scribe-test wait-output "$SESSION" "ai-phase1-ok"
echo "PHASE 1 PASS: processing state sent without disturbing terminal output"

# ── Phase 2: All five states cycle correctly ──────────────────────────────
# Emit each state, then a visible marker.  If any event were malformed the
# helper would still exit 0, but the marker must appear and the grid must
# stay intact.
for STATE in processing idle_prompt waiting_for_input permission_prompt error; do
    scribe-test send "$SESSION" "$HELPER --provider=claude_code --event=state_changed --state=$STATE; echo state-$STATE-ok\n"
    scribe-test wait-output "$SESSION" "state-$STATE-ok"
done
echo "PHASE 2 PASS: all five AI states cycled without corruption"

# ── Phase 3: Optional context metadata reaches the client chrome ─────────────
# The screen snapshot carries the server's PTY grid only, so the percentage is
# read back through `scribe-test ai-chrome`, which formats the session's live AI
# state with the same `scribe_common::ai_chrome` module the clients' prompt bar
# uses.
hold_ai_state "$HELPER --provider=claude_code --event=state_changed --state=processing; $HELPER --provider=claude_code --event=context_changed --fill-percent=42"
if ! chrome_wait "42%"; then
    echo "PHASE 3 FAIL: context=42 not rendered as 42% in the prompt bar"
    printf 'chrome was:\n%s\n' "$CHROME"
    exit 1
fi
release_ai_state
echo "PHASE 3 PASS: optional fields accepted, context % rendered"

# ── Phase 4: Interleaved output preserved across a legacy OSC ────────────────
# OSC 1337 no longer carries AI state, but the terminal must still consume the
# sequence silently and keep the text on either side of it.
scribe-test send "$SESSION" 'printf "BEFORE\\033]1337;ClaudeState=processing\\033\\\\AFTER\\n"\n'
scribe-test wait-output "$SESSION" "BEFORE"
scribe-test wait-output "$SESSION" "AFTER"
echo "PHASE 4 PASS: interleaved output preserved across OSC"

# ── Phase 5: Rapid state transitions ─────────────────────────────────────
# Emit several state changes in quick succession followed by a visible
# marker — the server must not drop output or deadlock.
scribe-test send "$SESSION" "for s in processing idle_prompt waiting_for_input permission_prompt processing error; do $HELPER --provider=claude_code --event=state_changed --state=\$s; done; echo rapid-ok\n"
scribe-test wait-output "$SESSION" "rapid-ok"
echo "PHASE 5 PASS: rapid state transitions handled without deadlock"

# ── Phase 6: An explicit clear drops the AI chrome ────────────────────────────
hold_ai_state "$HELPER --provider=claude_code --event=state_changed --state=processing; $HELPER --provider=claude_code --event=context_changed --fill-percent=66"
if ! chrome_wait "66%"; then
    echo "PHASE 6 FAIL: context=66 never appeared, cannot test the clear"
    printf 'chrome was:\n%s\n' "$CHROME"
    exit 1
fi
release_ai_state
scribe-test send "$SESSION" "$HELPER --provider=claude_code --event=state_cleared; echo cleared-ok\n"
scribe-test wait-output "$SESSION" "cleared-ok"
sleep 0.5
CHROME=$(scribe-test ai-chrome "$SESSION" 2>/dev/null || true)
if [ -n "$CHROME" ]; then
    echo "PHASE 6 FAIL: AI chrome still present after state_cleared"
    printf 'chrome was:\n%s\n' "$CHROME"
    exit 1
fi
echo "PHASE 6 PASS: state_cleared clears the AI chrome"

# ── Phase 7: Session with AI state can be closed cleanly ─────────────────
# This is last because closing a session may affect daemon state.
EXTRA=$(scribe-test session create)
scribe-test send "$EXTRA" "$HELPER --provider=claude_code --event=state_changed --state=processing; echo extra-alive\n"
scribe-test wait-output "$EXTRA" "extra-alive"
scribe-test session close "$EXTRA"
echo "PHASE 7 PASS: session with active AI state closed cleanly"

echo "PASS: AI state indicator test completed"
