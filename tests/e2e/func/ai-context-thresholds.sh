#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# AI Context Thresholds — functional E2E test
#
# Validates that the prompt-bar right cluster and tab-inline % display
# correctly across the three context threshold bands:
#   Ok     (< 70)  — only prompt bar shows %, tab suppresses it
#   Warn   (>= 70) — both prompt bar and tab show %
#   Danger (>= 90) — both prompt bar and tab show %
#
# Transport: AI state, prompt text, and context-window % reach the server over
# the hook channel (spec 003, FR-020..FR-022) — `scribe-hook-helper` invoked
# inside the session shell, where scribe-server exports SCRIBE_HOOK_SOCK and
# SCRIBE_SESSION_ID. OSC 1337 no longer carries any of it.
#
# Readback: `scribe-test ai-chrome` renders the session's live AI state through
# `scribe_common::ai_chrome`, the module the clients' prompt bar and tab bar
# format through. A terminal screen snapshot cannot be used: it carries the
# server's PTY grid only, and client chrome is never part of it.
# =============================================================================

HELPER=scribe-hook-helper

# Fire hook events in the session shell, then park the shell in `read` so it
# never prints a new prompt. A prompt (OSC 133;A) tells the server the AI tool
# exited, which clears the live AI state — exactly what the helper just set.
# This mirrors production, where hooks fire while the AI tool owns the
# foreground and the shell prompt is not being redrawn.
hold_ai_state() {
    local provider="$1" percent="$2" label="$3"
    scribe-test send "$SESSION" "$HELPER --provider=$provider --event=state_changed --state=processing; $HELPER --provider=$provider --event=prompt_received --text=$label; $HELPER --provider=$provider --event=context_changed --fill-percent=$percent; read -r\n"
}

# Release the parked shell; the returning prompt clears the AI state, so each
# phase starts from a clean slate.
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

# Assert a percentage renders on exactly the surfaces its band calls for:
# $2 = expected number of chrome lines mentioning it (1 = prompt bar only,
# 2 = prompt bar + tab).
assert_band() {
    local pct="$1" want="$2" phase="$3" count
    if ! chrome_wait "$pct"; then
        echo "$phase FAIL: context not rendered as $pct in the AI chrome"
        printf 'chrome was:\n%s\n' "$CHROME"
        exit 1
    fi
    count=$(printf '%s\n' "$CHROME" | grep -c -- "$pct" || true)
    if [ "$count" -ne "$want" ]; then
        echo "$phase FAIL: $pct on ${count} surface(s) — expected ${want}"
        printf 'chrome was:\n%s\n' "$CHROME"
        exit 1
    fi
}

# ── Phase 1 + 4: Ok band (50%) — prompt bar only ─────────────────────────────
# At 50% (below warn=70) the prompt bar renders "50%" but the tab-inline
# suppresses it, so "50%" appears on exactly one chrome surface.
hold_ai_state claude_code 50 phase-one
assert_band "50%" 1 "PHASE 1"
release_ai_state
echo "PHASE 1 PASS: context=50 (Ok band) rendered as 50% in prompt bar"
echo "PHASE 4 PASS: 50% on 1 surface (tab suppressed below warn threshold)"

# ── Phase 2: Warn band (72%) — prompt bar + tab ───────────────────────────────
hold_ai_state claude_code 72 phase-two
assert_band "72%" 2 "PHASE 2"
release_ai_state
echo "PHASE 2 PASS: 72% on 2 surfaces (prompt bar + tab both rendered)"

# ── Phase 3: Danger band (91%) — prompt bar + tab ────────────────────────────
hold_ai_state claude_code 91 phase-three
assert_band "91%" 2 "PHASE 3"
release_ai_state
echo "PHASE 3 PASS: 91% on 2 surfaces (prompt bar + tab both rendered)"

# ── Phase 5: Codex Ok band (51%) — prompt bar only ────────────────────────────
# Provider-symmetric: the same bands must hold for Codex.
hold_ai_state codex_code 51 phase-five
assert_band "51%" 1 "PHASE 5"
release_ai_state
echo "PHASE 5 PASS: Codex context=51 (Ok band) rendered as 51% in prompt bar"

# ── Phase 6: Codex Warn band (73%) — prompt bar + tab ──────────────────────────
hold_ai_state codex_code 73 phase-six
assert_band "73%" 2 "PHASE 6"
release_ai_state
echo "PHASE 6 PASS: Codex 73% on 2 surfaces (prompt bar + tab both rendered)"

# ── Phase 7: Codex Danger band (92%) — prompt bar + tab ─────────────────────────
hold_ai_state codex_code 92 phase-seven
assert_band "92%" 2 "PHASE 7"
release_ai_state
echo "PHASE 7 PASS: Codex 92% on 2 surfaces (prompt bar + tab both rendered)"

echo "ai-context-thresholds: all phases passed"
