#!/bin/bash
# Smart Claude state detection for Scribe terminal indicators.
#
# Called by the Stop hook. Reads JSON from stdin, inspects
# last_assistant_message, and emits:
#   waiting_for_input — if response ends with a question
#   idle_prompt       — otherwise
#
# Heuristics (in priority order):
#   1. Last non-empty line (outside a fenced code block) ends with "?"
#   2. Last paragraph contains question phrases ("Would you like",
#      "Should I", "Do you want", "Which option", etc.)

set -euo pipefail

STATE="idle_prompt"

# Read the full JSON payload from stdin.
INPUT=$(cat)

# Extract last_assistant_message via jq (fall back to idle_prompt on failure).
MSG=$(printf '%s' "$INPUT" | jq -r '.last_assistant_message // ""' 2>/dev/null) || MSG=""

if [[ -n "$MSG" ]]; then
    # Strip fenced code blocks (``` ... ```) so questions inside code
    # don't trigger a false positive.  Uses awk to toggle a flag.
    STRIPPED=$(printf '%s\n' "$MSG" | awk '
        /^```/ { inside = !inside; next }
        !inside { print }
    ')

    # Grab the last non-blank line and trim trailing whitespace.
    LAST_LINE=$(printf '%s\n' "$STRIPPED" \
        | sed '/^[[:space:]]*$/d' \
        | tail -n1 \
        | sed 's/[[:space:]]*$//')

    if [[ "$LAST_LINE" == *'?' ]]; then
        STATE="waiting_for_input"
    fi
fi

printf '\e]1337;ClaudeState=%s\a' "$STATE" > /dev/tty
