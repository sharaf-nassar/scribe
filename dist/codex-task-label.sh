#!/bin/bash
# Manage Scribe's Codex task-label metadata channel.
#
# `session-start` resets any tty-scoped task label/cache for a new Codex run.
# `user-prompt-submit` emits a task label when the prompt starts a new task.
# `tool-processing` emits the processing state for Bash tool hooks.

set -euo pipefail

ACTION="${1:-}"

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

CACHE_DIR="${HOME}/.codex/hooks/.scribe-task-label-cache"
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
    if ! has_tty; then
        return
    fi
    if [[ -n "$CACHE_FILE" ]]; then
        mkdir -p "$CACHE_DIR"
        rm -f "$CACHE_FILE"
    fi
    emit_osc 'CodexTaskLabelCleared'
}

handle_session_start() {
    drain_stdin
    reset_task_label
    emit_osc 'CodexState=idle_prompt'
}

handle_user_prompt_submit() {
    local payload parsed session_id label command
    payload="$(read_payload)"
    if ! has_tty; then
        return
    fi

    emit_osc 'CodexState=processing'
    parsed="$(
        PAYLOAD="$payload" python3 - <<'PY'
import json
import os
import re
import sys

MAX_LABEL_LEN = 120

try:
    payload = json.loads(os.environ.get("PAYLOAD", ""))
except json.JSONDecodeError:
    sys.exit(0)

prompt = payload.get("prompt")
session_id = payload.get("session_id")

if not isinstance(prompt, str) or not isinstance(session_id, str):
    sys.exit(0)

first_line = ""
for raw_line in prompt.splitlines():
    stripped = raw_line.strip()
    if stripped:
        first_line = stripped
        break

if not first_line:
    sys.exit(0)

if first_line.startswith("/new"):
    print("command=reset")
    sys.exit(0)

if first_line.startswith("/"):
    sys.exit(0)

normalized = "".join(ch if ch.isprintable() else " " for ch in first_line)
normalized = normalized.replace(";", ",")
normalized = re.sub(r"\s+", " ", normalized).strip()
if not normalized:
    sys.exit(0)

print(f"session_id={session_id}")
print(f"label={normalized[:MAX_LABEL_LEN]}")
PY
    )"

    if [[ -z "$parsed" ]]; then
        return
    fi

    session_id=""
    label=""
    command=""
    while IFS='=' read -r key value; do
        case "$key" in
            session_id) session_id="$value" ;;
            label) label="$value" ;;
            command) command="$value" ;;
        esac
    done <<< "$parsed"

    if [[ "$command" == "reset" ]]; then
        reset_task_label
        return
    fi

    if [[ -z "$session_id" || -z "$label" ]]; then
        return
    fi

    if [[ -n "$CACHE_FILE" ]] && [[ -f "$CACHE_FILE" ]] && [[ "$(cat "$CACHE_FILE")" == "$session_id" ]]; then
        return
    fi

    if [[ -n "$CACHE_FILE" ]]; then
        mkdir -p "$CACHE_DIR"
        printf '%s' "$session_id" > "$CACHE_FILE"
    fi
    emit_osc "CodexTaskLabel=${label}"
}

handle_tool_processing() {
    drain_stdin
    if ! has_tty; then
        return
    fi
    emit_osc 'CodexState=processing'
}

case "$ACTION" in
    session-start) handle_session_start ;;
    user-prompt-submit) handle_user_prompt_submit ;;
    tool-processing) handle_tool_processing ;;
    *) exit 0 ;;
esac
