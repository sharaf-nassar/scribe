#!/bin/bash
set -e

scribe-test send "$SESSION" 'echo scribe-e2e-test\n'
scribe-test wait-output "$SESSION" "scribe-e2e-test"
scribe-test screenshot "$SESSION" /output/01-echo.png

# Verify directory change (use wait-output instead of wait-cwd since
# the container shell may not emit OSC 7 sequences)
scribe-test send "$SESSION" 'cd /tmp && pwd\n'
scribe-test wait-output "$SESSION" "/tmp"
scribe-test screenshot "$SESSION" /output/02-cd.png

# Verify resize
scribe-test resize "$SESSION" 80 24
scribe-test wait-idle "$SESSION" --ms 300
scribe-test send "$SESSION" 'tput cols\n'
scribe-test wait-output "$SESSION" "80"
scribe-test screenshot "$SESSION" /output/03-resize.png

echo "PASS: smoke test completed"
