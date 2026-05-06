#!/bin/bash
# Manage Scribe's Auggie state and task-label metadata channel.
#
# Auggie does not document a UserPromptSubmit hook. Prompt and task-label
# metadata are best-effort from Stop conversation metadata when available.

set -euo pipefail

ACTION="${1:-}"
SCRIBE_HOOK_PAYLOAD="${SCRIBE_HOOK_PAYLOAD:-}"

TTY_PATH="$(
    python3 - <<'PY'
import os

try:
    fd = os.open("/dev/tty", os.O_RDWR)
except OSError:
    raise SystemExit(0)

try:
    print(os.ttyname(fd))
except OSError:
    pass
finally:
    os.close(fd)
PY
)"

CACHE_DIR="${HOME}/.augment/hooks/.scribe-task-label-cache"
CACHE_FILE=""
if [[ -n "$TTY_PATH" ]]; then
    CACHE_KEY="$(
        python3 - "$TTY_PATH" <<'PY'
import re
import sys

print(re.sub(r"[^A-Za-z0-9._-]+", "_", sys.argv[1]))
PY
    )"
    CACHE_FILE="${CACHE_DIR}/${CACHE_KEY}.thread"
fi

has_tty() {
    [[ -n "$TTY_PATH" ]]
}

read_payload() {
    if [[ -n "${SCRIBE_HOOK_PAYLOAD}" ]]; then
        printf '%s' "$SCRIBE_HOOK_PAYLOAD"
        return
    fi
    if [[ -t 0 ]]; then
        printf ''
    else
        cat || true
    fi
}

drain_stdin() {
    if [[ ! -t 0 ]]; then
        cat >/dev/null || true
    fi
}

emit_osc() {
    if ! has_tty; then
        return
    fi
    printf '\e]1337;%s\a' "$1" > /dev/tty 2>/dev/null || true
}

reset_task_label() {
    if [[ -n "$CACHE_FILE" ]]; then
        mkdir -p "$CACHE_DIR"
        rm -f "$CACHE_FILE"
    fi
    emit_osc 'AuggieTaskLabelCleared'
}

emit_state() {
    local state="$1" payload="$2" conversation_id=""
    conversation_id="$(
        PAYLOAD="$payload" python3 - <<'PY'
import json
import os
import sys

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except json.JSONDecodeError:
    sys.exit(0)

conversation_id = payload.get("conversation_id")
if isinstance(conversation_id, str) and conversation_id:
    print(conversation_id)
PY
    )"

    if [[ -n "$conversation_id" ]]; then
        emit_osc "AuggieState=${state};conversation_id=${conversation_id}"
    else
        emit_osc "AuggieState=${state}"
    fi
}

handle_session_start() {
    local payload
    payload="$(read_payload)"
    reset_task_label
    emit_state "idle_prompt" "$payload"
}

handle_session_end() {
    drain_stdin
    reset_task_label
    # Tell Scribe the AI tool went inactive so `ai_provider` clears and the
    # ED 3 filter stops applying to subsequent plain-shell bytes.
    emit_osc "AuggieState=inactive"
}

handle_processing() {
    local payload
    payload="$(read_payload)"
    emit_state "processing" "$payload"
}

handle_stop() {
    local payload parsed state conversation_id prompt label
    payload="$(read_payload)"

    parsed="$(
        PAYLOAD="$payload" python3 - <<'PY'
import json
import os
import re
import sys

MAX_PROMPT_LEN = 256
MAX_LABEL_LEN = 120

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except json.JSONDecodeError:
    payload = {}

conversation_id = payload.get("conversation_id")
conversation = payload.get("conversation")
if not isinstance(conversation, dict):
    conversation = {}

message = conversation.get("agentTextResponse")
if not isinstance(message, str):
    message = ""

state = "idle_prompt"
if message:
    stripped_lines = []
    inside_code = False
    for line in message.splitlines():
        if line.startswith("```"):
            inside_code = not inside_code
            continue
        if not inside_code and line.strip():
            stripped_lines.append(line)
    tail = "\n".join(stripped_lines[-20:])
    if re.search(r"\?\s*$", tail, re.MULTILINE):
        state = "waiting_for_input"
    elif re.search(r"(would you like|should i|do you want|which option|please (choose|select|pick)|how (should|would|do|to)|what (should|would|do)|let me know|your (choice|preference|call))", tail, re.I):
        state = "waiting_for_input"
    elif re.search(r"(please (review|approve)|review and approve|once approved|approve (the|this|it|above)|waiting for.*(approval|review)|I.ll execute.*once approved|confirm (the|this|before)|ready to (proceed|execute|start)|proceed\?)", tail, re.I):
        state = "waiting_for_input"

print(f"state={state}")
if isinstance(conversation_id, str) and conversation_id:
    print(f"conversation_id={conversation_id}")

prompt = conversation.get("userPrompt")
if isinstance(prompt, str):
    prompt = prompt.replace("\x07", "").replace("\x1b", "")
    prompt = re.sub(r"\s+", " ", prompt).strip()
    if prompt:
        print(f"prompt={prompt[:MAX_PROMPT_LEN]}")

        first_line = ""
        for raw_line in prompt.splitlines():
            stripped = raw_line.strip()
            if stripped:
                first_line = stripped
                break

        if first_line and not first_line.startswith("/"):
            normalized = "".join(ch if ch.isprintable() else " " for ch in first_line)
            normalized = normalized.replace(";", ",")
            normalized = re.sub(r"\s+", " ", normalized).strip()
            if normalized:
                print(f"label={normalized[:MAX_LABEL_LEN]}")
PY
    )"

    state="idle_prompt"
    conversation_id=""
    prompt=""
    label=""
    while IFS='=' read -r key value; do
        case "$key" in
            state) state="$value" ;;
            conversation_id) conversation_id="$value" ;;
            prompt) prompt="$value" ;;
            label) label="$value" ;;
        esac
    done <<< "$parsed"

    if [[ -n "$conversation_id" ]]; then
        emit_osc "AuggieState=${state};conversation_id=${conversation_id}"
    else
        emit_osc "AuggieState=${state}"
    fi

    if [[ -n "$prompt" ]]; then
        emit_osc "AuggiePrompt=${prompt}"
    fi

    if [[ -n "$conversation_id" && -n "$label" ]]; then
        if [[ -n "$CACHE_FILE" ]] && [[ -f "$CACHE_FILE" ]] && [[ "$(cat "$CACHE_FILE")" == "$conversation_id" ]]; then
            return
        fi
        if [[ -n "$CACHE_FILE" ]]; then
            mkdir -p "$CACHE_DIR"
            printf '%s' "$conversation_id" > "$CACHE_FILE"
        fi
        emit_osc "AuggieTaskLabel=${label}"
    fi
}

if [[ -z "$ACTION" ]]; then
    SCRIBE_HOOK_PAYLOAD="$(read_payload)"
    ACTION="$(
        PAYLOAD="$SCRIBE_HOOK_PAYLOAD" python3 - <<'PY'
import json
import os
import sys

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except json.JSONDecodeError:
    sys.exit(0)

event = payload.get("hook_event_name")
if event == "SessionStart":
    print("session-start")
elif event == "SessionEnd":
    print("session-end")
elif event == "Stop":
    print("stop")
elif event in ("PreToolUse", "PostToolUse"):
    print("processing")
PY
    )"
fi

case "$ACTION" in
    session-start) handle_session_start ;;
    session-end) handle_session_end ;;
    processing) handle_processing ;;
    stop) handle_stop ;;
    *) exit 0 ;;
esac
