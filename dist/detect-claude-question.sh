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

    # Extract the last paragraph (block of non-empty lines after the
    # final blank line).  A question followed by bullet-point options
    # lives in a single paragraph, so we must check the whole block.
    LAST_PARA=$(printf '%s\n' "$STRIPPED" | awk '
        /^[[:space:]]*$/ { para = ""; next }
        { para = para (para ? "\n" : "") $0 }
        END { print para }
    ')

    # Heuristic 1: any line in the last paragraph ends with "?"
    if printf '%s\n' "$LAST_PARA" | grep -qE '\?\s*$'; then
        STATE="waiting_for_input"
    # Heuristic 2: last paragraph contains common question phrases
    elif printf '%s\n' "$LAST_PARA" \
        | grep -qiE '(would you like|should i|do you want|which option|please (choose|select|pick)|how (should|would|do)|what (should|would|do)|let me know|your (choice|preference|call))'; then
        STATE="waiting_for_input"
    fi
fi

printf '\e]1337;ClaudeState=%s\a' "$STATE" > /dev/tty
