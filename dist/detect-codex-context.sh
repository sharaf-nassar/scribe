#!/usr/bin/env bash
#
# Scribe — Codex context % producer (PostToolUse / Stop hook)
#
# Reads the tail of the most recent rollout JSONL under ~/.codex/sessions/,
# finds the newest event_msg/token_count record, computes context %, and
# emits OSC 1337 CodexContext=NN to /dev/tty. The OSC carries no state —
# Codex's state transitions are owned by detect-codex-question.sh /
# codex-task-label.sh / setup-codex-hooks.sh emitters; this producer only
# patches the context-window %.
# Fails closed — exits 0 on every error path so Codex is never disrupted.
#
set -euo pipefail

# Drain stdin — Codex requires it. Capture to env-var so Python heredoc can
# own /dev/stdin if it needs to.
PAYLOAD="$(cat || true)"
export PAYLOAD

PAYLOAD="$PAYLOAD" python3 - <<'PY' || exit 0
import datetime, glob, json, os, sys

# Fallback window — used only when model_context_window is absent or
# non-positive in the rollout record.  The rollout's own field is authoritative.
DEFAULT_WINDOW = 200_000

home = os.path.expanduser("~")
# C1: prefer today's UTC date dir for O(today) scan; fall back to full recursive glob
today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y/%m/%d")
fast_pattern = os.path.join(home, ".codex", "sessions", today, "*.jsonl")
files = sorted(glob.glob(fast_pattern), key=os.path.getmtime)
if not files:
    fallback_pattern = os.path.join(home, ".codex", "sessions", "**", "*.jsonl")
    files = sorted(glob.glob(fallback_pattern, recursive=True), key=os.path.getmtime)
if not files:
    sys.exit(0)
latest = files[-1]

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
        # C3: prefer model_context_window from the record itself
        mw = info.get("model_context_window")
        if isinstance(mw, int) and mw > 0:
            window = mw
        # C2: read total_token_usage.total_tokens (running cumulative sum)
        ttu = info.get("total_token_usage")
        if isinstance(ttu, dict):
            t = ttu.get("total_tokens")
            if isinstance(t, int) and t > 0:
                total = t
                break
        # fallback: last_token_usage.total_tokens
        ltu = info.get("last_token_usage")
        if isinstance(ltu, dict):
            t = ltu.get("total_tokens")
            if isinstance(t, int) and t > 0:
                total = t
                break
except OSError:
    sys.exit(0)

if total <= 0:
    sys.exit(0)

# C3: if window not found in record, fall back to default
if window <= 0:
    window = DEFAULT_WINDOW

pct = max(0, min(100, round(100 * total / window)))
try:
    with open("/dev/tty", "w") as tty:
        tty.write(f"\x1b]1337;CodexContext={pct}\x07")
except OSError:
    pass
PY
