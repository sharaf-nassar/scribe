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

# Echo `{"<out_key>": <field>}` for the payload's `<in_key>` string field,
# or nothing when it is absent, null, non-string, or empty.
#
# Value-bearing fields never become arguments: /proc/<pid>/cmdline is
# world-readable, and a single argument is capped at MAX_ARG_STRLEN
# (128 KiB), which turned a long prompt into a silent E2BIG. Only the two
# fixed key names reach python's argv. This replaces an extract_field call
# rather than adding one, so the interpreter count per event is unchanged.
payload_field() {
    printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
v = d.get(sys.argv[1])
if not isinstance(v, str) or not v:
    sys.exit(0)
sys.stdout.write(json.dumps({sys.argv[2]: v}))
' "$1" "$2" 2>/dev/null
}

# Send one helper event with its payload document on stdin.
emit_payload() {
    payload="$1"
    shift
    printf '%s' "$payload" 2>/dev/null \
        | "$HELPER" "$@" --payload-stdin >/dev/null 2>&1
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
        PROMPT_PAYLOAD=$(payload_field prompt text)
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
        if [ -n "$PROMPT_PAYLOAD" ]; then
            if [ -n "$SID" ]; then
                emit_payload "$PROMPT_PAYLOAD" --provider=claude_code \
                    --event=prompt_received --conversation-id="$SID"
            else
                emit_payload "$PROMPT_PAYLOAD" --provider=claude_code \
                    --event=prompt_received
            fi
        fi
        exit 0
        ;;
    stop)
        # last_assistant_message can be many KiB and is model output the
        # user has not seen classified yet. It streams to the helper on
        # stdin, so it neither lands on disk (the old mktemp hand-off) nor
        # in argv, and the mktemp exec disappears with it.
        STOP_PAYLOAD=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    d = {}
msg = d.get("last_assistant_message")
sys.stdout.write(json.dumps({"last_message": msg if isinstance(msg, str) else ""}))
' 2>/dev/null) || STOP_PAYLOAD=""
        # No python3, or it died mid-write: still emit, so Stop keeps
        # classifying exactly as it did when the temp file came back empty.
        [ -n "$STOP_PAYLOAD" ] || STOP_PAYLOAD='{"last_message":""}'
        SID=$(extract_field session_id)
        if [ -n "$SID" ]; then
            emit_payload "$STOP_PAYLOAD" --provider=claude_code \
                --event=session_stopped --conversation-id="$SID"
        else
            emit_payload "$STOP_PAYLOAD" --provider=claude_code \
                --event=session_stopped
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac

exit 0
