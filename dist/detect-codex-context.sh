#!/usr/bin/env bash
#
# Scribe — Codex context % producer (PostToolUse / Stop hook)
#
# Reads Codex's hook `transcript_path`, extracts the latest
# event_msg/token_count record from that rollout JSONL, computes context %, and
# emits OSC 1337 CodexContext=NN to /dev/tty. The OSC carries no state —
# Codex's state transitions are owned by detect-codex-question.sh /
# codex-task-label.sh / setup-codex-hooks.sh emitters; this producer only
# patches the context-window %.
# Fails closed — exits 0 on every error path so Codex is never disrupted.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=dist/codex-hook-common.sh
. "${SCRIPT_DIR}/codex-hook-common.sh"

# Drain stdin — Codex requires it. Capture to env-var so Python heredoc can
# own /dev/stdin if it needs to.
PAYLOAD="$(scribe_codex_read_payload)"
if ! scribe_codex_owner_allows_payload "$PAYLOAD" 0; then
    exit 0
fi
export PAYLOAD

PAYLOAD="$PAYLOAD" python3 - <<'PY' || exit 0
import json, os, sys

home = os.path.expanduser("~")
sessions_root = os.path.join(home, ".codex", "sessions")

try:
    hook_payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except (json.JSONDecodeError, ValueError):
    sys.exit(0)
if not isinstance(hook_payload, dict):
    sys.exit(0)

transcript_path = hook_payload.get("transcript_path")
if not isinstance(transcript_path, str) or not transcript_path:
    sys.exit(0)


def safe_transcript_path(path):
    candidate = os.path.abspath(os.path.expanduser(path))
    try:
        root_real = os.path.realpath(sessions_root)
        candidate_real = os.path.realpath(candidate)
        if os.path.commonpath([root_real, candidate_real]) != root_real:
            return None
        if not candidate_real.endswith(".jsonl") or not os.path.isfile(candidate_real):
            return None
        return candidate_real
    except (OSError, ValueError):
        return None

latest = safe_transcript_path(transcript_path)
if latest is None:
    sys.exit(0)

total = 0
window = 0
try:
    with open(latest, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        chunk_size = min(size, 64 * 1024)
        f.seek(size - chunk_size)
        chunk = f.read(chunk_size)
    lines = chunk.splitlines()
    # I1: if we read a partial tail, the first line may be truncated — drop it
    if chunk_size < size:
        lines = lines[1:]
    # I2 / C2: walk newest-first; break on first usable token_count record
    for raw in reversed(lines):
        try:
            rec = json.loads(raw.decode("utf-8", "ignore"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        # C2: filter to event_msg / token_count records only
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
try:
    with open("/dev/tty", "w") as tty:
        tty.write(f"\x1b]1337;CodexContext={pct}\x07")
except OSError:
    pass
PY
