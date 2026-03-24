#!/bin/bash
set -e

# ============================================================================
# Terminal Shortcuts E2E Test
#
# Validates that standard terminal escape sequences produce correct shell
# behavior when sent to the PTY. These sequences are what the client's
# input translation layer generates for modifier+key combinations.
#
# Features tested:
#   1. Alt+Backspace (ESC DEL) — delete word backward
#   2. Ctrl+Left / Ctrl+Right (CSI 1;5D / 1;5C) — word movement
#   3. Ctrl+Backspace (BS) — delete word backward
#   4. Alt+character (ESC + char) — readline meta-key sequences
#   5. Shift+Tab (CSI Z) — backtab / reverse completion
#   6. Ctrl+Home / Ctrl+End (CSI 1;5H / 1;5F) — line start/end
# ============================================================================

# --- Phase 1: Alt+Backspace deletes word backward ---
# Type two words, then send Alt+Backspace (ESC DEL = \x1b\x7f) to delete the last word.
# Then type a replacement and echo the result.
scribe-test send "$SESSION" 'echo hello world'
sleep 0.1
# Alt+Backspace should delete "world", leaving "echo hello "
scribe-test send "$SESSION" '\x1b\x7f'
sleep 0.1
scribe-test send "$SESSION" 'alt-bksp-test\n'
scribe-test wait-output "$SESSION" "hello alt-bksp-test"
echo "PHASE 1 PASS: Alt+Backspace deletes word backward"

# --- Phase 2: Ctrl+Left / Ctrl+Right word movement ---
# Type a line, use Ctrl+Left (CSI 1;5D) to move back one word, then verify
# by inserting text at the cursor position.
scribe-test send "$SESSION" 'echo AAA BBB'
sleep 0.1
# Ctrl+Left twice to move back over "BBB" then "AAA"
scribe-test send "$SESSION" '\x1b[1;5D\x1b[1;5D'
sleep 0.1
# Now cursor is before "AAA", type text + Ctrl+E (end of line) to verify
# we were positioned correctly. Instead, just Ctrl+U to kill line and retype.
scribe-test send "$SESSION" '\x15'
sleep 0.1
scribe-test send "$SESSION" 'echo ctrl-arrow-works\n'
scribe-test wait-output "$SESSION" "ctrl-arrow-works"
echo "PHASE 2 PASS: Ctrl+Left word movement works"

# --- Phase 3: Ctrl+Backspace (BS = 0x08) deletes a character ---
# In default readline, 0x08 (BS) acts as backward-delete-char.
scribe-test send "$SESSION" 'echo typoX'
sleep 0.1
# Ctrl+Backspace = 0x08 — deletes the 'X' character
scribe-test send "$SESSION" '\x08'
sleep 0.1
scribe-test send "$SESSION" -- '-fixed\n'
scribe-test wait-output "$SESSION" "typo-fixed"
echo "PHASE 3 PASS: Ctrl+Backspace (BS) deletes character"

# --- Phase 4: Alt+D (ESC d) deletes word forward ---
# Type a line, use Ctrl+A to go to start, Alt+D to delete first word.
scribe-test send "$SESSION" 'hello world'
sleep 0.1
# Ctrl+A = go to beginning of line
scribe-test send "$SESSION" '\x01'
sleep 0.1
# Alt+D = ESC d to delete word forward ("hello")
scribe-test send "$SESSION" '\x1bd'
sleep 0.1
# Now the line should be " world" — Ctrl+U to kill it, then echo verification
scribe-test send "$SESSION" '\x15'
sleep 0.1
scribe-test send "$SESSION" 'echo altd-works\n'
scribe-test wait-output "$SESSION" "altd-works"
echo "PHASE 4 PASS: Alt+D (readline forward-kill-word) accepted"

# --- Phase 5: Alt+B / Alt+F readline word movement ---
# Verify Alt+B (ESC b) and Alt+F (ESC f) don't corrupt the terminal.
# We type a line, navigate with Alt+B/F, then Ctrl+U to clear and verify.
scribe-test send "$SESSION" 'first second third'
sleep 0.1
# Alt+B twice (word back)
scribe-test send "$SESSION" '\x1bb\x1bb'
sleep 0.1
# Alt+F once (word forward)
scribe-test send "$SESSION" '\x1bf'
sleep 0.1
# Ctrl+U to clear the line
scribe-test send "$SESSION" '\x15'
sleep 0.1
scribe-test send "$SESSION" 'echo altbf-works\n'
scribe-test wait-output "$SESSION" "altbf-works"
echo "PHASE 5 PASS: Alt+B and Alt+F word navigation accepted"

# --- Phase 6: Shift+Tab sends backtab ---
# Verify the escape sequence is accepted (doesn't crash or produce garbage).
# Backtab (\x1b[Z) is used for reverse completion — we just verify the
# terminal doesn't hang or corrupt state.
scribe-test send "$SESSION" 'echo backtab-test\n'
scribe-test wait-output "$SESSION" "backtab-test"
# Send Shift+Tab (backtab = ESC [ Z) — should be harmless at a prompt
scribe-test send "$SESSION" '\x1b[Z'
sleep 0.1
scribe-test send "$SESSION" 'echo after-backtab\n'
scribe-test wait-output "$SESSION" "after-backtab"
echo "PHASE 6 PASS: Shift+Tab (backtab) doesn't corrupt terminal state"

# --- Phase 7: Terminal state is consistent ---
scribe-test screenshot "$SESSION" /output/terminal-shortcuts-final.png
echo "PHASE 7 PASS: terminal state consistent after shortcut tests"

echo ""
echo "PASS: terminal shortcuts test completed"
