#!/bin/sh
# Scribe — Auggie AI hook adapter.
#
# Translates Augment Auggie's hook stdin JSON into one or more invocations
# of scribe-hook-helper. Auggie does not document a UserPromptSubmit hook;
# prompt and task-label metadata arrive on `Stop` via `includeConversationData`.
#
# Usage:
#   ai-hook-auggie.sh <event-name>
#
# Recognized event-name values: session_start, session_end, processing, stop.

set +e
EVENT_NAME="${1:-}"

HELPER="${SCRIBE_HOOK_HELPER:-$(dirname "$0")/scribe-hook-helper}"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe-dev/scribe-hook-helper"
[ -x "$HELPER" ] || { cat >/dev/null 2>&1; exit 0; }

PAYLOAD=$(cat 2>/dev/null) || PAYLOAD=""

extract_conv_id() {
    printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
c = d.get("conversation_id")
if isinstance(c, str) and c:
    # Strip newlines/CRs defensively so the value cannot inject
    # extra key=value lines into the shell parser downstream and
    # cannot smuggle ANSI/control chars into the Scribe UI.
    sys.stdout.write(c.replace("\n", "").replace("\r", ""))
' 2>/dev/null
}

case "$EVENT_NAME" in
    session_start)
        "$HELPER" --provider=auggie --event=task_label_cleared \
            </dev/null >/dev/null 2>&1
        CID=$(extract_conv_id)
        if [ -n "$CID" ]; then
            "$HELPER" --provider=auggie --event=state_changed \
                --state=idle_prompt --conversation-id="$CID" \
                </dev/null >/dev/null 2>&1
        else
            "$HELPER" --provider=auggie --event=state_changed \
                --state=idle_prompt </dev/null >/dev/null 2>&1
        fi
        exit 0
        ;;
    session_end)
        # Auggie session ended — clear AI state so subsequent plain-shell
        # bytes stop being treated as Auggie output.
        "$HELPER" --provider=auggie --event=task_label_cleared \
            </dev/null >/dev/null 2>&1
        exec "$HELPER" --provider=auggie --event=state_cleared \
            </dev/null >/dev/null 2>&1
        ;;
    processing)
        # PreToolUse / PostToolUse — Auggie is mid-turn.
        CID=$(extract_conv_id)
        if [ -n "$CID" ]; then
            exec "$HELPER" --provider=auggie --event=state_changed \
                --state=processing --conversation-id="$CID" \
                </dev/null >/dev/null 2>&1
        else
            exec "$HELPER" --provider=auggie --event=state_changed \
                --state=processing </dev/null >/dev/null 2>&1
        fi
        ;;
    stop)
        # Stop hook with includeConversationData=true: extract
        # agentTextResponse (for session_stopped + classifier),
        # userPrompt (for prompt_received), and derive task label.
        TMPFILE=$(mktemp 2>/dev/null) || exit 0
        PARSED=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, os, re, sys

MAX_PROMPT_LEN = 256
MAX_LABEL_LEN = 120

try:
    d = json.loads(sys.stdin.read() or "{}")
except Exception:
    d = {}

_raw_cid = d.get("conversation_id")
cid = _raw_cid.replace("\n", "").replace("\r", "") if isinstance(_raw_cid, str) else ""
conv = d.get("conversation") if isinstance(d.get("conversation"), dict) else {}

# Write agent message to the temp file for last_message_file.
msg = conv.get("agentTextResponse")
if isinstance(msg, str):
    with open(os.environ["TMPFILE"], "w") as f:
        f.write(msg)

# Print prompt + label to stdout (key=value lines).
print(f"conversation_id={cid}")

prompt = conv.get("userPrompt")
if isinstance(prompt, str):
    cleaned = prompt.replace("\x07", "").replace("\x1b", "")
    cleaned = re.sub(r"\s+", " ", cleaned).strip()
    if cleaned:
        print(f"prompt={cleaned[:MAX_PROMPT_LEN]}")

        first = ""
        for raw in cleaned.splitlines():
            s = raw.strip()
            if s:
                first = s
                break
        if first and not first.startswith("/"):
            norm = "".join(ch if ch.isprintable() else " " for ch in first)
            norm = norm.replace(";", ",")
            norm = re.sub(r"\s+", " ", norm).strip()
            if norm:
                print(f"label={norm[:MAX_LABEL_LEN]}")
' 2>/dev/null) || PARSED=""
        export TMPFILE

        CID=""
        PROMPT=""
        LABEL=""
        OLDIFS="$IFS"
        IFS='
'
        for line in $PARSED; do
            case "$line" in
                conversation_id=*) CID="${line#conversation_id=}" ;;
                prompt=*)          PROMPT="${line#prompt=}" ;;
                label=*)           LABEL="${line#label=}" ;;
            esac
        done
        IFS="$OLDIFS"

        # session_stopped (with classifier picking idle vs waiting).
        if [ -s "$TMPFILE" ]; then
            if [ -n "$CID" ]; then
                "$HELPER" --provider=auggie --event=session_stopped \
                    --last-message-file="$TMPFILE" --conversation-id="$CID" \
                    </dev/null >/dev/null 2>&1
            else
                "$HELPER" --provider=auggie --event=session_stopped \
                    --last-message-file="$TMPFILE" \
                    </dev/null >/dev/null 2>&1
            fi
        else
            # No agent message — best-effort idle_prompt.
            rm -f "$TMPFILE"
            if [ -n "$CID" ]; then
                "$HELPER" --provider=auggie --event=state_changed \
                    --state=idle_prompt --conversation-id="$CID" \
                    </dev/null >/dev/null 2>&1
            else
                "$HELPER" --provider=auggie --event=state_changed \
                    --state=idle_prompt </dev/null >/dev/null 2>&1
            fi
        fi

        if [ -n "$PROMPT" ]; then
            if [ -n "$CID" ]; then
                "$HELPER" --provider=auggie --event=prompt_received \
                    --text="$PROMPT" --conversation-id="$CID" \
                    </dev/null >/dev/null 2>&1
            else
                "$HELPER" --provider=auggie --event=prompt_received \
                    --text="$PROMPT" </dev/null >/dev/null 2>&1
            fi
        fi

        if [ -n "$LABEL" ]; then
            "$HELPER" --provider=auggie --event=task_label_changed \
                --label="$LABEL" </dev/null >/dev/null 2>&1
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac

exit 0
