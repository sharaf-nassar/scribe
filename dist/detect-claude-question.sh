#!/bin/bash
# Smart Claude state detection for Scribe terminal indicators.
#
# Called by the Stop hook. Reads JSON from stdin, inspects
# last_assistant_message, and emits:
#   waiting_for_input — if response ends with a question or approval request
#   idle_prompt       — otherwise
#
# Heuristics (checked against the last ~20 non-empty lines, not just the
# last paragraph, so questions followed by a concluding sentence are caught):
#   1. Any line ends with "?"
#   2. Contains question phrases ("Would you like", "Should I", etc.)
#   3. Contains approval/review phrases ("please review", "once approved", etc.)

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

    # Take the last 20 non-empty lines instead of just the last paragraph.
    # This catches questions that are followed by a concluding sentence in
    # a separate paragraph (e.g. "These answers will help me..." after
    # numbered questions ending with "?").
    TAIL_LINES=$(printf '%s\n' "$STRIPPED" | grep -v '^[[:space:]]*$' | tail -n 20)

    # Heuristic 1: any line in the tail ends with "?"
    if printf '%s\n' "$TAIL_LINES" | grep -qE '\?\s*$'; then
        STATE="waiting_for_input"
    # Heuristic 2: tail contains common question phrases
    elif printf '%s\n' "$TAIL_LINES" \
        | grep -qiE '(would you like|should i|do you want|which option|please (choose|select|pick)|how (should|would|do)|what (should|would|do)|let me know|your (choice|preference|call))'; then
        STATE="waiting_for_input"
    # Heuristic 3: tail contains approval/review request phrases
    elif printf '%s\n' "$TAIL_LINES" \
        | grep -qiE '(please (review|approve)|review and approve|once approved|approve (the|this|it|above)|waiting for.*(approval|review)|I.ll execute.*once approved|confirm (the|this|before)|ready to (proceed|execute|start)|proceed\?)'; then
        STATE="waiting_for_input"
    fi
fi

printf '\e]1337;ClaudeState=%s\a' "$STATE" > /dev/tty
