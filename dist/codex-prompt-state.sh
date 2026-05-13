#!/bin/bash
# Emit Scribe Codex prompt metadata for the TTY owner session only.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=dist/codex-hook-common.sh
. "${SCRIPT_DIR}/codex-hook-common.sh"

PAYLOAD="$(scribe_codex_read_payload)"
export PAYLOAD

if [[ "$(scribe_codex_prompt_starts_new "$PAYLOAD")" == "1" ]]; then
    scribe_codex_owner_reset
    exit 0
fi

if ! scribe_codex_owner_allows_payload "$PAYLOAD" 1; then
    exit 0
fi

python3 - <<'PY' || true
import json
import os

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except (json.JSONDecodeError, ValueError):
    raise SystemExit(0)

session_id = payload.get("session_id", "")
prompt = payload.get("prompt", "")

if not isinstance(session_id, str):
    session_id = ""
if not isinstance(prompt, str):
    raise SystemExit(0)

prompt = prompt[:256].replace(chr(7), "").replace(chr(27), "")
if not prompt:
    raise SystemExit(0)

try:
    with open("/dev/tty", "w") as tty:
        if session_id:
            tty.write(f"\x1b]1337;CodexState=processing;conversation_id={session_id}\x07")
        else:
            tty.write("\x1b]1337;CodexState=processing\x07")
        tty.write(f"\x1b]1337;CodexPrompt={prompt}\x07")
except OSError:
    pass
PY
