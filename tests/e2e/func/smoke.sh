#!/bin/bash
set -e

# ── Phase 1: Basic I/O ───────────────────────────────────────────
scribe-test send "$SESSION" 'echo scribe-e2e-test\n'
scribe-test wait-output "$SESSION" "scribe-e2e-test"
echo "PHASE 1 PASS: basic echo I/O"

# ── Phase 2: Cursor position ─────────────────────────────────────
# After a command, the cursor should be on a new line at col 0 (or after
# the prompt). We verify cursor_row advanced past row 0.
scribe-test send "$SESSION" 'echo cursor-check\n'
scribe-test wait-output "$SESSION" "cursor-check"
scribe-test wait-idle "$SESSION" --ms 200
# Cursor should NOT be on row 0 (we've output at least 2 lines by now).
ROW=$(scribe-test snapshot "$SESSION" /dev/stdout 2>/dev/null | grep -oP '"cursor_row": \K[0-9]+')
if [ "$ROW" -gt 0 ] 2>/dev/null; then
    echo "PHASE 2 PASS: cursor advanced to row $ROW"
else
    echo "PHASE 2 FAIL: cursor stuck at row 0"
    exit 1
fi

# ── Phase 3: Cell content verification ────────────────────────────
# Write a known character at a predictable position using cursor addressing.
scribe-test send "$SESSION" 'printf "\\033[1;1H#"\n'
scribe-test wait-idle "$SESSION" --ms 200
scribe-test assert-cell "$SESSION" 0 0 '#'
echo "PHASE 3 PASS: assert-cell verified cursor-addressed write"

# ── Phase 4: Unicode pass-through ─────────────────────────────────
scribe-test send "$SESSION" 'echo "Unicode: cafe\\xcc\\x81"\n'
scribe-test wait-output "$SESSION" "Unicode:"
echo "PHASE 4 PASS: Unicode output received"

# ── Phase 5: Environment variable propagation ─────────────────────
scribe-test send "$SESSION" 'echo "TERM=$TERM"\n'
scribe-test wait-output "$SESSION" "TERM=xterm-256color"
echo "PHASE 5 PASS: TERM=xterm-256color propagated"

# ── Phase 6: Ctrl+C signal passthrough ────────────────────────────
# Start a command that blocks, send Ctrl+C, verify the shell recovers.
scribe-test send "$SESSION" 'sleep 999\n'
scribe-test wait-idle "$SESSION" --ms 200
scribe-test send "$SESSION" '\x03'
scribe-test wait-idle "$SESSION" --ms 300
scribe-test send "$SESSION" 'echo survived-sigint\n'
scribe-test wait-output "$SESSION" "survived-sigint"
echo "PHASE 6 PASS: Ctrl+C interrupted sleep, shell recovered"

# ── Phase 7: Resize round-trip ────────────────────────────────────
scribe-test resize "$SESSION" 120 40
scribe-test wait-idle "$SESSION" --ms 300
scribe-test send "$SESSION" 'tput cols\n'
scribe-test wait-output "$SESSION" "120"

scribe-test resize "$SESSION" 80 24
scribe-test wait-idle "$SESSION" --ms 300
scribe-test send "$SESSION" 'tput cols\n'
scribe-test wait-output "$SESSION" "80"
echo "PHASE 7 PASS: resize 120x40 then back to 80x24"

# ── Phase 8: Directory change ─────────────────────────────────────
scribe-test send "$SESSION" 'cd /tmp && pwd\n'
scribe-test wait-output "$SESSION" "/tmp"
echo "PHASE 8 PASS: cd + pwd verified"

# ── Phase 9: Session create + close lifecycle ─────────────────────
EXTRA=$(scribe-test session create)
scribe-test send "$EXTRA" 'echo extra-alive\n'
scribe-test wait-output "$EXTRA" "extra-alive"
scribe-test session close "$EXTRA"
echo "PHASE 9 PASS: second session created, used, and closed"

echo "PASS: smoke test completed"
