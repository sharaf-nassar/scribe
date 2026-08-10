#!/usr/bin/env bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#AI Indicator E2E#Codex Subagent Hook Isolation E2E]]
set -euo pipefail

ADAPTER=/usr/local/share/scribe/ai-hook-codex.sh
FIXTURE="$HOME/.codex/sessions/scribe-subagent-e2e-$$"
CAPTURE="$FIXTURE/helper-events"
STUB="$FIXTURE/scribe-hook-helper"
ROOT_TRANSCRIPT="$FIXTURE/root.jsonl"
CHILD_TRANSCRIPT="$FIXTURE/child.jsonl"
trap 'rm -rf "$FIXTURE"' EXIT
mkdir -p "$FIXTURE/runtime"

printf '%s\n' \
    '#!/bin/sh' \
    'payload=$(cat)' \
    'printf "%s|%s\n" "$*" "$payload" >>"$SCRIBE_HOOK_CAPTURE"' \
    >"$STUB"
chmod +x "$STUB"

printf '%s\n' \
    '{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":100000,"last_token_usage":{"total_tokens":43000}}}}' \
    >"$ROOT_TRANSCRIPT"
printf '%s\n' \
    '{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":100000,"last_token_usage":{"total_tokens":93000}}}}' \
    >"$CHILD_TRANSCRIPT"

run_hook() {
    local event="$1" payload="$2"
    printf '%s' "$payload" | \
        SCRIBE_HOOK_HELPER="$STUB" \
        SCRIBE_HOOK_CAPTURE="$CAPTURE" \
        XDG_RUNTIME_DIR="$FIXTURE/runtime" \
        "$ADAPTER" "$event"
}

run_hook post_tool_use \
    "{\"transcript_path\":\"$ROOT_TRANSCRIPT\",\"subagent\":null}"
run_hook post_tool_use \
    "{\"transcript_path\":\"$CHILD_TRANSCRIPT\",\"subagent\":{\"thread_id\":\"child\"}}"
run_hook pre_tool_use '{"subagent":null}'
run_hook pre_tool_use '{"subagent":{"thread_id":"child"}}'

EXPECTED=$'--provider=codex_code --event=state_changed --payload-stdin|{"state":"processing"}\n--provider=codex_code --event=context_changed --payload-stdin|{"fill_percent":43}\n--provider=codex_code --event=state_changed --payload-stdin|{"state":"processing"}'
ACTUAL=$(cat "$CAPTURE")
if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "FAIL: child Codex hooks changed the parent event stream"
    printf 'expected:\n%s\nactual:\n%s\n' "$EXPECTED" "$ACTUAL"
    exit 1
fi

echo "PASS: root context stayed at 43% and child PostToolUse/PreToolUse emitted nothing"
