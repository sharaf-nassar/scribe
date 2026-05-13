#!/bin/bash
# Manage Scribe's Codex task-label metadata channel.
#
# `session-start` resets any tty-scoped task label/cache for a new Codex run.
# `user-prompt-submit` emits a task label when the prompt starts a new task.
# `tool-processing` emits the processing state for Bash tool hooks.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=dist/codex-hook-common.sh
. "${SCRIPT_DIR}/codex-hook-common.sh"

drain_stdin() {
    if [[ ! -t 0 ]]; then
        cat >/dev/null || true
    fi
}

ACTION="${1:-}"

reset_task_label() {
    if ! scribe_codex_has_tty; then
        return
    fi
    if [[ -n "$SCRIBE_CODEX_LABEL_CACHE_FILE" ]]; then
        mkdir -p "$SCRIBE_CODEX_CACHE_DIR"
        rm -f "$SCRIBE_CODEX_LABEL_CACHE_FILE"
    fi
    scribe_codex_emit_osc 'CodexTaskLabelCleared'
}

handle_session_start() {
    local payload
    payload="$(scribe_codex_read_payload)"
    if ! scribe_codex_owner_allows_payload "$payload" 1; then
        return
    fi
    reset_task_label
    if [[ "${SCRIBE_CODEX_OWNER_CLAIMED:-0}" == "1" ]]; then
        scribe_codex_emit_osc 'CodexState=idle_prompt'
    fi
}

handle_user_prompt_submit() {
    local payload parsed session_id label command
    payload="$(scribe_codex_read_payload)"
    if ! scribe_codex_has_tty; then
        return
    fi
    if ! scribe_codex_owner_allows_payload "$payload" 1; then
        return
    fi

    scribe_codex_emit_osc 'CodexState=processing'
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
        scribe_codex_owner_reset
        reset_task_label
        return
    fi

    if [[ -z "$session_id" || -z "$label" ]]; then
        return
    fi

    if [[ -n "$SCRIBE_CODEX_LABEL_CACHE_FILE" ]] && [[ -f "$SCRIBE_CODEX_LABEL_CACHE_FILE" ]] && [[ "$(cat "$SCRIBE_CODEX_LABEL_CACHE_FILE")" == "$session_id" ]]; then
        return
    fi

    if [[ -n "$SCRIBE_CODEX_LABEL_CACHE_FILE" ]]; then
        mkdir -p "$SCRIBE_CODEX_CACHE_DIR"
        printf '%s' "$session_id" > "$SCRIBE_CODEX_LABEL_CACHE_FILE"
    fi
    scribe_codex_emit_osc "CodexTaskLabel=${label}"
}

handle_tool_processing() {
    local payload
    payload="$(scribe_codex_read_payload)"
    if ! scribe_codex_has_tty; then
        return
    fi
    if ! scribe_codex_owner_allows_payload "$payload" 0; then
        return
    fi
    scribe_codex_emit_osc 'CodexState=processing'
}

case "$ACTION" in
    session-start) handle_session_start ;;
    user-prompt-submit) handle_user_prompt_submit ;;
    tool-processing) handle_tool_processing ;;
    *) exit 0 ;;
esac
