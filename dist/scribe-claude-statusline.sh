#!/bin/bash
set -euo pipefail
#
# Scribe — Claude Code statusLine producer
#
# CC invokes this script with a JSON document on stdin whenever the status
# line needs to be refreshed. This script:
#   1. Reads context_window.used_percentage (or falls back to computing it
#      from token counts) and emits an OSC 1337 ClaudeContext=NN sequence
#      to /dev/tty so Scribe can update the live context-window % without
#      ever asserting a state. State transitions are owned exclusively by
#      Claude's hook scripts (PreToolUse / PostToolUse / Stop / etc.).
#      The previous version of this script emitted ClaudeState=processing
#      on every refresh, which clobbered idle/waiting states immediately
#      after the Stop hook fired and left CC panes "stuck in processing".
#   2. Writes a human-readable banner to stdout that CC renders as the
#      visible status line.
#
# Older CC versions may omit context_window entirely — handled defensively.
# Exits 0 on every error path so CC is never disrupted.

PAYLOAD="$(cat || true)"

PAYLOAD="$PAYLOAD" python3 - <<'PYEOF'
import json
import os
import sys

try:
    data = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except (json.JSONDecodeError, ValueError):
    sys.exit(0)

# ── Extract percentage ────────────────────────────────────────────────────
pct = None

cw = data.get("context_window")
if isinstance(cw, dict):
    used = cw.get("used_percentage")
    if isinstance(used, (int, float)):
        pct = int(round(used))
    else:
        # Fall back: compute from token counts vs window size
        size = cw.get("context_window_size")
        total_in = cw.get("total_input_tokens")
        total_out = cw.get("total_output_tokens")
        if (
            isinstance(size, (int, float))
            and size > 0
            and isinstance(total_in, (int, float))
            and isinstance(total_out, (int, float))
        ):
            pct = int(round((total_in + total_out) / size * 100))

if pct is not None:
    pct = max(0, min(100, pct))

# ── Extract model display name ────────────────────────────────────────────
model_name = None
model = data.get("model")
if isinstance(model, dict):
    name = model.get("display_name")
    if isinstance(name, str) and name:
        model_name = name

# ── Emit OSC to /dev/tty (best effort) ───────────────────────────────────
if pct is not None:
    osc = f"\x1b]1337;ClaudeContext={pct}\x07"
    try:
        with open("/dev/tty", "w") as tty:
            tty.write(osc)
    except OSError:
        pass

# ── Write banner to stdout ────────────────────────────────────────────────
if model_name and pct is not None:
    print(f"{model_name} • {pct}%")
elif model_name:
    print(model_name)
# else: emit nothing (CC hides the status line)

sys.exit(0)
PYEOF
