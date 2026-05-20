#!/bin/sh
# Scribe — Codex AI hook adapter.
#
# Translates Codex's hook stdin JSON into one or more invocations of
# scribe-hook-helper. Called by Codex's hook system with one positional
# argument identifying which hook event fired.
#
# Contract (per specs/003-ai-hook-channel/contracts/helper-cli.md):
#   - exit 0 in EVERY code path (FR-007)
#   - never write to stdout (FR-008)
#   - never write to stderr (FR-009)
#   - never open /dev/tty (FR-010)
#
# Usage:
#   ai-hook-codex.sh <event-name>
#
# Recognized event-name values: session_start, user_prompt_submit,
# permission_request, tool_processing, stop, context.

set +e
EVENT_NAME="${1:-}"

# Helper resolution: try explicit env var, then sibling-to-this-script
# for Linux package layouts, then the macOS app-bundle MacOS directory.
HELPER="${SCRIBE_HOOK_HELPER:-$(dirname "$0")/scribe-hook-helper}"
[ -x "$HELPER" ] || HELPER="$(dirname "$0")/../MacOS/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe-dev/scribe-hook-helper"
[ -x "$HELPER" ] || { cat >/dev/null 2>&1; exit 0; }

# Payload is piped into the inline python heredocs that need it; no need to
# export to the helper's env (large messages would waste env-block space).
PAYLOAD=$(cat 2>/dev/null) || PAYLOAD=""

extract_field() {
    printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
v = d.get(sys.argv[1])
if not isinstance(v, str):
    sys.exit(0)
sys.stdout.write(v)
' "$1" 2>/dev/null
}

case "$EVENT_NAME" in
    session_start)
        # New Codex session: clear task label, then emit idle_prompt.
        "$HELPER" --provider=codex_code --event=task_label_cleared \
            </dev/null >/dev/null 2>&1
        SID=$(extract_field session_id)
        if [ -n "$SID" ]; then
            "$HELPER" --provider=codex_code --event=state_changed \
                --state=idle_prompt --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1
        else
            "$HELPER" --provider=codex_code --event=state_changed \
                --state=idle_prompt </dev/null >/dev/null 2>&1
        fi
        exit 0
        ;;
    user_prompt_submit)
        # Codex emitted UserPromptSubmit: state → processing, then derive a
        # task label from the prompt's first non-empty line (skipping slash
        # commands).
        SID=$(extract_field session_id)
        PROMPT=$(extract_field prompt)
        if [ -n "$SID" ]; then
            "$HELPER" --provider=codex_code --event=state_changed \
                --state=processing --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1
        else
            "$HELPER" --provider=codex_code --event=state_changed \
                --state=processing </dev/null >/dev/null 2>&1
        fi

        if [ -n "$PROMPT" ]; then
            if [ -n "$SID" ]; then
                "$HELPER" --provider=codex_code --event=prompt_received \
                    --text="$PROMPT" --conversation-id="$SID" \
                    </dev/null >/dev/null 2>&1
            else
                "$HELPER" --provider=codex_code --event=prompt_received \
                    --text="$PROMPT" </dev/null >/dev/null 2>&1
            fi
        fi

        LABEL=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, re, sys
try:
    d = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
prompt = d.get("prompt", "")
if not isinstance(prompt, str):
    sys.exit(0)
first = ""
for raw in prompt.splitlines():
    s = raw.strip()
    if s:
        first = s
        break
if not first or first.startswith("/"):
    sys.exit(0)
normalized = "".join(ch if ch.isprintable() else " " for ch in first)
normalized = normalized.replace(";", ",")
normalized = re.sub(r"\s+", " ", normalized).strip()
if normalized:
    sys.stdout.write(normalized[:120])
' 2>/dev/null) || LABEL=""

        if [ -n "$LABEL" ]; then
            "$HELPER" --provider=codex_code --event=task_label_changed \
                --label="$LABEL" </dev/null >/dev/null 2>&1
        fi
        exit 0
        ;;
    permission_request)
        # Codex is about to ask for approval. Surface the attention state in
        # Scribe without deciding the request for Codex.
        SID=$(extract_field session_id)
        if [ -n "$SID" ]; then
            exec "$HELPER" --provider=codex_code --event=state_changed \
                --state=permission_prompt --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1
        else
            exec "$HELPER" --provider=codex_code --event=state_changed \
                --state=permission_prompt </dev/null >/dev/null 2>&1
        fi
        ;;
    tool_processing)
        # PreToolUse / PostToolUse → keep state on processing.
        exec "$HELPER" --provider=codex_code --event=state_changed \
            --state=processing </dev/null >/dev/null 2>&1
        ;;
    stop)
        # Codex Stop: classify via session_stopped + last_message file.
        # Helper unlinks the file on success; we clean up if exec fails.
        TMPFILE=$(mktemp 2>/dev/null) || exit 0
        printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
msg = d.get("last_assistant_message")
if not isinstance(msg, str):
    sys.exit(0)
sys.stdout.write(msg)
' > "$TMPFILE" 2>/dev/null
        SID=$(extract_field session_id)
        if [ -n "$SID" ]; then
            "$HELPER" --provider=codex_code --event=session_stopped \
                --last-message-file="$TMPFILE" --conversation-id="$SID" \
                </dev/null >/dev/null 2>&1 || rm -f "$TMPFILE"
        else
            "$HELPER" --provider=codex_code --event=session_stopped \
                --last-message-file="$TMPFILE" \
                </dev/null >/dev/null 2>&1 || rm -f "$TMPFILE"
        fi
        exit 0
        ;;
    context)
        # PostToolUse / Stop context % refresh — parse the last token_count
        # event in the rollout transcript JSONL.
        PCT=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, os, sys

home = os.path.expanduser("~")
sessions_root = os.path.join(home, ".codex", "sessions")

try:
    d = json.loads(sys.stdin.read() or "{}")
except Exception:
    sys.exit(0)
if not isinstance(d, dict):
    sys.exit(0)

transcript_path = d.get("transcript_path")
if not isinstance(transcript_path, str) or not transcript_path:
    sys.exit(0)

candidate = os.path.abspath(os.path.expanduser(transcript_path))
try:
    root_real = os.path.realpath(sessions_root)
    candidate_real = os.path.realpath(candidate)
    if os.path.commonpath([root_real, candidate_real]) != root_real:
        sys.exit(0)
    if not candidate_real.endswith(".jsonl") or not os.path.isfile(candidate_real):
        sys.exit(0)
except (OSError, ValueError):
    sys.exit(0)

total = 0
window = 0
try:
    with open(candidate_real, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        chunk_size = min(size, 64 * 1024)
        f.seek(size - chunk_size)
        chunk = f.read(chunk_size)
    lines = chunk.splitlines()
    if chunk_size < size:
        lines = lines[1:]
    for raw in reversed(lines):
        try:
            rec = json.loads(raw.decode("utf-8", "ignore"))
        except Exception:
            continue
        if rec.get("type") != "event_msg":
            continue
        payload = rec.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "token_count":
            continue
        info = payload.get("info")
        if not isinstance(info, dict):
            continue
        mw = info.get("model_context_window")
        if isinstance(mw, int) and mw > 0:
            window = mw
        ltu = info.get("last_token_usage")
        if isinstance(ltu, dict):
            t = ltu.get("total_tokens")
            if isinstance(t, int) and t > 0:
                total = t
                break
except OSError:
    sys.exit(0)

if total <= 0 or window <= 0:
    sys.exit(0)

pct = max(0, min(100, round(100 * total / window)))
sys.stdout.write(str(pct))
' 2>/dev/null) || PCT=""

        if [ -n "$PCT" ]; then
            exec "$HELPER" --provider=codex_code --event=context_changed \
                --fill-percent="$PCT" </dev/null >/dev/null 2>&1
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac

exit 0
