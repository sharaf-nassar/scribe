#!/bin/bash
set -e

# ── Phase 1: Verify shell integration environment ────────────────
scribe-test wait-idle "$SESSION" --ms 500

scribe-test send "$SESSION" 'echo $SCRIBE_SHELL_INTEGRATION\n'
scribe-test wait-output "$SESSION" "^1$"
echo "PHASE 1 PASS: SCRIBE_SHELL_INTEGRATION=1"

# ── Phase 2: Verify TERM_PROGRAM ─────────────────────────────────
scribe-test send "$SESSION" 'echo $TERM_PROGRAM\n'
scribe-test wait-output "$SESSION" "Scribe"
echo "PHASE 2 PASS: TERM_PROGRAM=Scribe"

# ── Phase 3: Verify colored completions (readline) ───────────────
scribe-test send "$SESSION" 'bind -v 2>/dev/null | grep colored-stats\n'
scribe-test wait-output "$SESSION" "colored-stats"
echo "PHASE 3 PASS: colored-stats enabled"

# ── Phase 4: Verify CWD reporting via OSC 7 ─────────────────────
scribe-test send "$SESSION" 'cd /tmp\n'
scribe-test wait-idle "$SESSION" --ms 300

scribe-test send "$SESSION" 'pwd\n'
scribe-test wait-output "$SESSION" "/tmp"
echo "PHASE 4 PASS: CWD reporting via OSC 7 (cd /tmp, pwd shows /tmp)"

# ── Phase 5: Screenshot for visual verification ──────────────────
scribe-test screenshot "$SESSION" /output/shell-integration.png
echo "PHASE 5 PASS: screenshot saved"

echo "PASS: shell-integration test completed"
