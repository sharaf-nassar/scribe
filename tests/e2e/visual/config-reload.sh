#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: live config reload in the running GPUI terminal window.
#
# Backs the `ClientMessage::ConfigReloaded` parity row. The client is already
# running against the server when this script starts, so editing config.toml
# here is exactly the user-visible scenario: save the file, the window changes,
# nothing restarts. The script asserts all three legs — the client process is
# the same one that started (no restart), its watcher logged a hot reload, and
# the painted window actually changed.
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CONFIG_DIR="$XDG_CONFIG_HOME/scribe"
CONFIG_FILE="$CONFIG_DIR/config.toml"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
mkdir -p "$CONFIG_DIR"

capture_window() {
    local out="$1"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.3
    fi
    scrot "$out"
}

# Wait for a line to appear in the client log, up to `timeout` seconds.
wait_for_log() {
    local pattern="$1" timeout_secs="${2:-10}" started
    started=$(date +%s)
    while true; do
        if grep -qF "$pattern" "$CLIENT_LOG" 2>/dev/null; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.2
    done
}

# ── Phase 1: baseline — window painted with the startup config ─────
sleep 0.8
capture_window /output/01-config-baseline.png
PID_BEFORE=$(pgrep -f '(^|/)scribe-client$' | head -1)
if [ -z "$PID_BEFORE" ]; then
    echo "PHASE 1 FAIL: no running scribe-client process"
    exit 1
fi
RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
echo "PHASE 1 PASS: baseline captured (client pid $PID_BEFORE)"

# ── Phase 2: edit the config on disk while the window is up ────────
# Theme, font metrics, and a keybinding all change at once so a single save
# exercises every branch of the reload plan.
cat > "$CONFIG_FILE" <<'EOF'
[appearance]
theme = "dracula"
font = "JetBrains Mono"
font_size = 22.0
line_padding = 6
opacity = 0.85

[keybindings]
command_palette = ["ctrl+shift+o"]
EOF
echo "PHASE 2 PASS: config.toml rewritten under the running client"

# ── Phase 3: the watcher picked the edit up without a restart ──────
if ! wait_for_log "config hot-reloaded" 10; then
    echo "PHASE 3 FAIL: client never logged a config hot-reload"
    tail -30 "$CLIENT_LOG" || true
    exit 1
fi
RELOADS_AFTER=$(grep -cF "config hot-reloaded" "$CLIENT_LOG")
if [ "$RELOADS_AFTER" -le "${RELOADS_BEFORE:-0}" ]; then
    echo "PHASE 3 FAIL: no new hot-reload after the edit"
    exit 1
fi
echo "PHASE 3 PASS: config watcher reloaded the running window"

# ── Phase 4: it was a reload, not a restart ────────────────────────
PID_AFTER=$(pgrep -f '(^|/)scribe-client$' | head -1)
if [ "$PID_AFTER" != "$PID_BEFORE" ]; then
    echo "PHASE 4 FAIL: client pid changed ($PID_BEFORE -> $PID_AFTER); it restarted"
    exit 1
fi
echo "PHASE 4 PASS: same client process ($PID_AFTER) — no restart"

# ── Phase 5: the painted window actually changed ───────────────────
sleep 0.6
capture_window /output/02-config-reloaded.png
if cmp -s /output/01-config-baseline.png /output/02-config-reloaded.png; then
    echo "PHASE 5 FAIL: window is pixel-identical after the theme/font edit"
    exit 1
fi
echo "PHASE 5 PASS: window repainted with the new theme and font"

# ── Phase 6: the new keybinding is live ────────────────────────────
# The palette combo moved to ctrl+shift+o in the edited file. Pressing it must
# open the command palette even though the binding did not exist at startup.
WID=$(xdotool search --name "Scribe" | head -1) || true
if [ -n "$WID" ]; then
    xdotool windowfocus --sync "$WID" 2>/dev/null || true
    xdotool key --window "$WID" ctrl+shift+o
    sleep 0.5
fi
capture_window /output/03-reloaded-keybinding.png
if cmp -s /output/02-config-reloaded.png /output/03-reloaded-keybinding.png; then
    echo "PHASE 6 FAIL: the hot-reloaded palette binding did nothing"
    exit 1
fi
echo "PHASE 6 PASS: keybinding edit took effect without a restart"

echo ""
echo "PASS: visual config-reload test"
echo "  Inspect screenshots in test-output/:"
echo "    01-config-baseline.png     — window before the config edit"
echo "    02-config-reloaded.png     — new theme + font applied live"
echo "    03-reloaded-keybinding.png — palette opened by the new combo"
