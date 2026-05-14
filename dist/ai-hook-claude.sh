#!/bin/sh
# Scribe — Claude Code AI hook adapter.
#
# Translates Claude Code's hook stdin JSON into one or more invocations of
# scribe-hook-helper. Called by Claude Code's hook system with one positional
# argument identifying which hook event fired.
#
# Contract (per specs/003-ai-hook-channel/contracts/helper-cli.md):
#   - exit 0 in EVERY code path (FR-007)
#   - never write to stdout (FR-008)
#   - never write to stderr (FR-009)
#   - never open /dev/tty (FR-010)
#
# Usage:
#   ai-hook-claude.sh <event-name>
#
# Recognized event-name values: permission_prompt, error,
# pre_ask_user_question, post_ask_user_question, user_prompt_submit, stop.

set +e
EVENT_NAME="${1:-}"

# Helper resolution: try explicit env var, then sibling-to-this-script
# for Linux package layouts, then the macOS app-bundle MacOS directory.
HELPER="${SCRIBE_HOOK_HELPER:-$(dirname "$0")/scribe-hook-helper}"
[ -x "$HELPER" ] || HELPER="$(dirname "$0")/../MacOS/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe-dev/scribe-hook-helper"
# If the helper is missing, bail silently. The helper itself also handles
# every failure mode silently, but this short-circuit avoids the
# python-extraction cost when nothing can be delivered.
[ -x "$HELPER" ] || { cat >/dev/null 2>&1; exit 0; }

# Read the full payload once. python3 is already a Scribe install dep used
# elsewhere in dist/. Any read failure → silent exit 0.
PAYLOAD=$(cat 2>/dev/null) || PAYLOAD=""

# Extract a single JSON string field. Echoes nothing on error: missing
# field, null value, non-string value (which would otherwise stringify to
# "None"), malformed payload, or missing python3.
extract_field() {
    printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
v = d.get(sys.argv[1])
if not isinstance(v, str):
    sys.exit(0)
sys.stdout.write(v)
' "$1" 2>/dev/null
}

case "$EVENT_NAME" in
    permission_prompt)
        exec "$HELPER" --provider=claude_code --event=state_changed \
            --state=permission_prompt </dev/null >/dev/null 2>&1
        ;;
    error)
        exec "$HELPER" --provider=claude_code --event=state_changed \
            --state=error </dev/null >/dev/null 2>&1
        ;;
    pre_ask_user_question)
        exec "$HELPER" --provider=claude_code --event=state_changed \
            --state=waiting_for_input </dev/null >/dev/null 2>&1
        ;;
    post_ask_user_question)
        exec "$HELPER" --provider=claude_code --event=state_changed \
            --state=processing </dev/null >/dev/null 2>&1
        ;;
    user_prompt_submit)
        PROMPT=$(extract_field prompt)
        SID=$(extract_field session_id)
        # Two events: state→processing, then prompt_received (if non-empty).
        if [ -n "$SID" ]; then
            "$HELPER" --provider=claude_code --event=state_changed \
                --state=processing --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1
        else
            "$HELPER" --provider=claude_code --event=state_changed \
                --state=processing </dev/null >/dev/null 2>&1
        fi
        if [ -n "$PROMPT" ]; then
            if [ -n "$SID" ]; then
                "$HELPER" --provider=claude_code --event=prompt_received \
                    --text="$PROMPT" --conversation-id="$SID" \
                    </dev/null >/dev/null 2>&1
            else
                "$HELPER" --provider=claude_code --event=prompt_received \
                    --text="$PROMPT" </dev/null >/dev/null 2>&1
            fi
        fi
        exit 0
        ;;
    stop)
        # last_assistant_message can be many KiB — pass via temp file to
        # avoid ARG_MAX. The helper unlinks the file after a successful
        # read; we clean up here if the helper fails to exec.
        TMPFILE=$(mktemp 2>/dev/null) || exit 0
        printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
msg = d.get("last_assistant_message")
if not isinstance(msg, str):
    sys.exit(0)
sys.stdout.write(msg)
' > "$TMPFILE" 2>/dev/null
        SID=$(extract_field session_id)
        if [ -n "$SID" ]; then
            "$HELPER" --provider=claude_code --event=session_stopped \
                --last-message-file="$TMPFILE" --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1 || rm -f "$TMPFILE"
        else
            "$HELPER" --provider=claude_code --event=session_stopped \
                --last-message-file="$TMPFILE" \
                </dev/null >/dev/null 2>&1 || rm -f "$TMPFILE"
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac

exit 0
