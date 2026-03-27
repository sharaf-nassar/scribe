#!/bin/bash
# Smart Codex state detection for Scribe terminal indicators.
#
# Called by the Codex Stop hook. Reads JSON from stdin, inspects
# last_assistant_message, and emits:
#   waiting_for_input — if response ends with a question or approval request
#   idle_prompt       — otherwise
#
# Stop hooks must not print plain text on stdout, so this script only writes
# the OSC sequence to /dev/tty as a side effect.

set -euo pipefail

STATE="idle_prompt"

# Read the full JSON payload from stdin.
INPUT=$(cat)

# Extract last_assistant_message via jq (fall back to idle_prompt on failure).
MSG=$(printf '%s' "$INPUT" | jq -r '.last_assistant_message // ""' 2>/dev/null) || MSG=""

if [[ -n "$MSG" ]]; then
    # Strip fenced code blocks so questions inside code do not trigger.
    STRIPPED=$(printf '%s\n' "$MSG" | awk '
        /^```/ { inside = !inside; next }
        !inside { print }
    ')

    # Inspect the tail of the response so questions followed by a summary
    # paragraph still count as "waiting for input".
    TAIL_LINES=$(printf '%s\n' "$STRIPPED" | grep -v '^[[:space:]]*$' | tail -n 20)

    if printf '%s\n' "$TAIL_LINES" | grep -qE '\?\s*$'; then
        STATE="waiting_for_input"
    elif printf '%s\n' "$TAIL_LINES" \
        | grep -qiE '(would you like|should i|do you want|which option|please (choose|select|pick)|how (should|would|do|to)|what (should|would|do)|let me know|your (choice|preference|call))'; then
        STATE="waiting_for_input"
    elif printf '%s\n' "$TAIL_LINES" \
        | grep -qiE '(please (review|approve)|review and approve|once approved|approve (the|this|it|above)|waiting for.*(approval|review)|I.ll execute.*once approved|confirm (the|this|before)|ready to (proceed|execute|start)|proceed\?)'; then
        STATE="waiting_for_input"
    fi
fi

printf '\e]1337;CodexState=%s\a' "$STATE" > /dev/tty
