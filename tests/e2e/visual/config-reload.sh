#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: an external config edit repaints the running GPUI client
# without restarting it. Settings keybinding and theme-picker suites own their
# richer feature-specific reload paths.
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
    scrot -o "$out"
}

wait_for_log() {
    local pattern="$1" timeout_secs="${2:-10}" started
    started=$(date +%s)
    while ! grep -qF "$pattern" "$CLIENT_LOG" 2>/dev/null; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.2
    done
}

sleep 0.8
capture_window /output/01-config-baseline.png
PID_BEFORE=$(pgrep -f '(^|/)scribe-client$' | head -1)
[ -n "$PID_BEFORE" ] || { echo "FAIL: no running scribe-client process"; exit 1; }
RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)

cat > "$CONFIG_FILE" <<'EOF'
[appearance]
theme = "dracula"
EOF

if ! wait_for_log "config hot-reloaded" 10; then
    echo "FAIL: client never logged a config hot-reload"
    tail -30 "$CLIENT_LOG" || true
    exit 1
fi
RELOADS_AFTER=$(grep -cF "config hot-reloaded" "$CLIENT_LOG")
[ "$RELOADS_AFTER" -gt "${RELOADS_BEFORE:-0}" ] \
    || { echo "FAIL: no new hot-reload after the edit"; exit 1; }

PID_AFTER=$(pgrep -f '(^|/)scribe-client$' | head -1)
[ "$PID_AFTER" = "$PID_BEFORE" ] \
    || { echo "FAIL: client restarted ($PID_BEFORE -> $PID_AFTER)"; exit 1; }

sleep 0.6
capture_window /output/02-config-reloaded.png
cmp -s /output/01-config-baseline.png /output/02-config-reloaded.png \
    && { echo "FAIL: window is pixel-identical after the theme edit"; exit 1; }

echo "PASS: external config edit hot-reloaded and repainted the existing client"
