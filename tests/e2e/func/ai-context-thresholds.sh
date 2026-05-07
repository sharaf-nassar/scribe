#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# AI Context Thresholds — functional E2E test
#
# Validates that the prompt-bar right cluster and tab-inline % display
# correctly across the three context threshold bands:
#   Ok     (< 70)  — only prompt bar shows %, tab suppresses it
#   Warn   (>= 70) — both prompt bar and tab show %, count >= 2
#   Danger (>= 90) — both prompt bar and tab show %, count >= 2
# =============================================================================

# ── Phase 1 + 4: Ok band (50%) — prompt bar only ─────────────────────────────
# At 50% (below warn=70) the prompt bar renders "50%" but the tab-inline
# suppresses it, so "50%" should appear exactly once in the snapshot.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing;context=50\\033\\\\\\033]1337;ClaudePrompt=phase-one\\033\\\\"; echo ctx-phase1-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase1-ok"

SNAP1=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP1" | grep -q "50%"; then
    echo "PHASE 1 FAIL: context=50 not rendered as 50% in prompt bar"
    exit 1
fi
echo "PHASE 1 PASS: context=50 (Ok band) rendered as 50% in prompt bar"

# Phase 4 assertion: tab suppression — 50% should appear at most once.
# (Tab-inline only activates at >= warn threshold; prompt bar always shows it.)
COUNT1=$(echo "$SNAP1" | grep -c "50%" || true)
if [ "$COUNT1" -le 1 ]; then
    echo "PHASE 4 PASS: 50% count=${COUNT1} (tab suppressed below warn threshold)"
else
    # Tolerate >1 only if a CWD or other harness artifact happens to contain "50%".
    # The invariant is that the TAB should not add a second mention; if this fires
    # it likely means tab-inline is not correctly gated on the warn threshold.
    echo "PHASE 4 FAIL: 50% count=${COUNT1} — expected <= 1 (tab should suppress below warn)"
    exit 1
fi

# ── Phase 2: Warn band (72%) — prompt bar + tab ───────────────────────────────
# At 72% (>= warn=70) both the prompt-bar segment AND the tab inline label
# should render "72%", so it should appear at least twice.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing;context=72\\033\\\\\\033]1337;ClaudePrompt=phase-two\\033\\\\"; echo ctx-phase2-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase2-ok"

SNAP2=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP2" | grep -q "72%"; then
    echo "PHASE 2 FAIL: context=72 not rendered as 72% anywhere in snapshot"
    exit 1
fi
COUNT2=$(echo "$SNAP2" | grep -c "72%" || true)
if [ "$COUNT2" -ge 2 ]; then
    echo "PHASE 2 PASS: 72% count=${COUNT2} >= 2 (prompt bar + tab both rendered)"
else
    echo "PHASE 2 FAIL: 72% count=${COUNT2} — expected >= 2 (prompt bar + tab inline)"
    exit 1
fi

# ── Phase 3: Danger band (91%) — prompt bar + tab ────────────────────────────
# At 91% (>= danger=90) both the prompt-bar segment AND the tab inline label
# should render "91%", so it should appear at least twice.
scribe-test send "$SESSION" 'printf "\\033]1337;ClaudeState=processing;context=91\\033\\\\\\033]1337;ClaudePrompt=phase-three\\033\\\\"; echo ctx-phase3-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase3-ok"

SNAP3=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP3" | grep -q "91%"; then
    echo "PHASE 3 FAIL: context=91 not rendered as 91% anywhere in snapshot"
    exit 1
fi
COUNT3=$(echo "$SNAP3" | grep -c "91%" || true)
if [ "$COUNT3" -ge 2 ]; then
    echo "PHASE 3 PASS: 91% count=${COUNT3} >= 2 (prompt bar + tab both rendered)"
else
    echo "PHASE 3 FAIL: 91% count=${COUNT3} — expected >= 2 (prompt bar + tab inline)"
    exit 1
fi

# ── Phase 5: Codex Ok band (51%) — prompt bar only ────────────────────────────
# Codex provider-symmetric test: send CodexState instead of ClaudeState.
# At 51% (below warn=70) the prompt bar renders "51%" but the tab-inline
# suppresses it, so "51%" should appear exactly once in the snapshot.
# (Use 51 instead of 50 to avoid collision with Phase 1's prior "50%" in scrollback.)
scribe-test send "$SESSION" 'printf "\\033]1337;CodexState=processing;context=51\\033\\\\\\033]1337;CodexPrompt=phase-five\\033\\\\"; echo ctx-phase5-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase5-ok"

SNAP5=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP5" | grep -q "51%"; then
    echo "PHASE 5 FAIL: CodexState with context=51 not rendered as 51% in prompt bar"
    exit 1
fi
COUNT5=$(echo "$SNAP5" | grep -c "51%" || true)
if [ "$COUNT5" -le 1 ]; then
    echo "PHASE 5 PASS: CodexState context=51 (Ok band) rendered as 51% in prompt bar"
else
    echo "PHASE 5 FAIL: 51% count=${COUNT5} — expected <= 1 (tab should suppress below warn)"
    exit 1
fi

# ── Phase 6: Codex Warn band (73%) — prompt bar + tab ──────────────────────────
# Codex provider-symmetric test: at 73% (>= warn=70) both the prompt-bar segment
# AND the tab inline label should render "73%", so it should appear at least twice.
# (Use 73 instead of 72 to avoid collision with Phase 2's prior "72%" in scrollback.)
scribe-test send "$SESSION" 'printf "\\033]1337;CodexState=processing;context=73\\033\\\\\\033]1337;CodexPrompt=phase-six\\033\\\\"; echo ctx-phase6-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase6-ok"

SNAP6=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP6" | grep -q "73%"; then
    echo "PHASE 6 FAIL: CodexState with context=73 not rendered as 73% anywhere in snapshot"
    exit 1
fi
COUNT6=$(echo "$SNAP6" | grep -c "73%" || true)
if [ "$COUNT6" -ge 2 ]; then
    echo "PHASE 6 PASS: CodexState 73% count=${COUNT6} >= 2 (prompt bar + tab both rendered)"
else
    echo "PHASE 6 FAIL: 73% count=${COUNT6} — expected >= 2 (prompt bar + tab inline)"
    exit 1
fi

# ── Phase 7: Codex Danger band (92%) — prompt bar + tab ─────────────────────────
# Codex provider-symmetric test: at 92% (>= danger=90) both the prompt-bar segment
# AND the tab inline label should render "92%", so it should appear at least twice.
# (Use 92 instead of 91 to avoid collision with Phase 3's prior "91%" in scrollback.)
scribe-test send "$SESSION" 'printf "\\033]1337;CodexState=processing;context=92\\033\\\\\\033]1337;CodexPrompt=phase-seven\\033\\\\"; echo ctx-phase7-ok\n'
scribe-test wait-output "$SESSION" "ctx-phase7-ok"

SNAP7=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null)
if ! echo "$SNAP7" | grep -q "92%"; then
    echo "PHASE 7 FAIL: CodexState with context=92 not rendered as 92% anywhere in snapshot"
    exit 1
fi
COUNT7=$(echo "$SNAP7" | grep -c "92%" || true)
if [ "$COUNT7" -ge 2 ]; then
    echo "PHASE 7 PASS: CodexState 92% count=${COUNT7} >= 2 (prompt bar + tab both rendered)"
else
    echo "PHASE 7 FAIL: 92% count=${COUNT7} — expected >= 2 (prompt bar + tab inline)"
    exit 1
fi

echo "ai-context-thresholds: all phases passed"
