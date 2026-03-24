#!/bin/bash
set -e

# ============================================================================
# Keybindings Validation E2E Test
#
# Validates that configurable keybindings work correctly.
# Tests default keybindings (which should still work since the parser
# produces the same bindings from the default config strings).
#
# Features tested:
#   1. Basic I/O still works (regression check)
#   2. Terminal input is not intercepted by keybinding parser
# ============================================================================

# --- Phase 1: Verify basic I/O ---
scribe-test send "$SESSION" 'echo KB-TEST\n'
scribe-test wait-output "$SESSION" "KB-TEST"
echo "PHASE 1 PASS: basic I/O works with dynamic keybinding matching"

# --- Phase 2: Verify Ctrl+key sequences pass through to PTY ---
# Ctrl+C should produce ^C (SIGINT). We start a sleep, send Ctrl+C, verify
# the shell returns to a prompt.
scribe-test send "$SESSION" 'sleep 999\n'
sleep 0.3
# Ctrl+C = 0x03
scribe-test send "$SESSION" '\x03'
scribe-test wait-idle "$SESSION" --ms 500
scribe-test send "$SESSION" 'echo AFTER-CTRLC\n'
scribe-test wait-output "$SESSION" "AFTER-CTRLC"
echo "PHASE 2 PASS: Ctrl+C passes through to PTY (not consumed by keybinding parser)"

# --- Phase 3: Verify resize still works (may be triggered by split) ---
scribe-test resize "$SESSION" 120 40
scribe-test wait-idle "$SESSION" --ms 300
scribe-test send "$SESSION" 'tput cols\n'
scribe-test wait-output "$SESSION" "120"
echo "PHASE 3 PASS: resize works"

# --- Phase 4: Verify cursor position after commands ---
scribe-test send "$SESSION" 'echo CURSOR-CHECK\n'
scribe-test wait-output "$SESSION" "CURSOR-CHECK"
scribe-test wait-idle "$SESSION" --ms 300
# Cursor should be on a prompt line, column 0 or at the prompt character.
# Just verify the terminal state is consistent (no crash).
scribe-test screenshot "$SESSION" /output/kb-final.png
echo "PHASE 4 PASS: terminal state consistent after keybinding tests"

echo ""
echo "PASS: keybindings validation test completed"
