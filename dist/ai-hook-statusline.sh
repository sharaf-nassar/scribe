#!/bin/sh
# Scribe — Claude Code statusLine subprocess.
#
# Registered as Claude Code's `statusLine.command`. Receives Claude's
# status-line JSON on stdin (containing `context_window.used_percentage`
# and `model.display_name`). Emits a context-fill event to Scribe via
# scribe-hook-helper, then prints a one-line banner to stdout — that
# banner is the literal text Claude Code shows in its status line.
#
# Replaces the OSC-over-/dev/tty path that broke when Claude Code v2.1.139
# detached the controlling terminal from hook and statusLine subprocesses.
#
# Contract:
#   - exit 0 always
#   - stderr: never written
#   - stdout: ONLY the human banner (CC consumes it; not a leakage vector)

set +e
HELPER="${SCRIBE_HOOK_HELPER:-$(dirname "$0")/scribe-hook-helper}"
[ -x "$HELPER" ] || HELPER="$(dirname "$0")/../MacOS/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe-dev/scribe-hook-helper"
PAYLOAD=$(cat 2>/dev/null) || PAYLOAD=""

# Extract (pct, model) as `pct|model` so a single python3 invocation pulls
# both. On any error, prints `|` so PCT and MODEL both become empty.
DATA=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("|")
    sys.exit(0)
ctx = d.get("context_window") or {}
pct_raw = ctx.get("used_percentage")
pct = ""
if pct_raw is not None:
    try:
        pct = str(max(0, min(100, int(round(float(pct_raw))))))
    except Exception:
        pct = ""
model = ""
m = d.get("model")
if isinstance(m, dict):
    model = str(m.get("display_name", "") or "")
print(f"{pct}|{model}")
' 2>/dev/null) || DATA="|"

PCT="${DATA%%|*}"
MODEL="${DATA#*|}"

# Best-effort: send context-fill to Scribe if we got a valid integer.
if [ -x "$HELPER" ] && [ -n "$PCT" ]; then
    "$HELPER" --provider=claude_code --event=context_changed \
        --fill-percent="$PCT" </dev/null >/dev/null 2>&1
fi

# Banner displayed by Claude Code in its status line. This is intentionally
# the only stdout write in this script.
if [ -n "$MODEL" ] && [ -n "$PCT" ]; then
    printf '%s — %s%% context\n' "$MODEL" "$PCT"
elif [ -n "$MODEL" ]; then
    printf '%s\n' "$MODEL"
elif [ -n "$PCT" ]; then
    printf '%s%% context\n' "$PCT"
fi

exit 0
