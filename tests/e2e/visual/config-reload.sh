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

# The Docker image must not supply the default face behind the package's back.
# Embedded fonts are private to GPUI and must not appear in fontconfig.
if fc-list : family | grep -q 'JetBrains Mono'; then
    echo "FAIL: clean-font fixture contaminated by a host JetBrains Mono installation"
    exit 1
fi
cat > "$CONFIG_FILE" <<'EOF'
[appearance]
theme = "dracula"
font = "JetBrains Mono"
cursor_blink = false
EOF
sleep 0.6
WID=$(xdotool search --name '^Scribe$' | head -1)
xdotool windowfocus --sync "$WID"
xdotool type --clearmodifiers --delay 2 'printf "\033[2J\033[H0123456789 0123456789\nMMMMMMMMMM iiiiiiiiii\n\033[1mBOLD       BOLD\033[0m\n\033[3mITALIC     ITALIC\033[0m\n"'
xdotool key --clearmodifiers Return
sleep 0.8
import -window "$WID" /output/03-bundled-font.png
# Skip chrome and the shell cursor. This captures only the four fixture rows.
convert /output/03-bundled-font.png -crop 350x72+8+40 +repage /output/bundled-font-grid.png
INK=$(convert /output/bundled-font-grid.png -colorspace Gray -threshold 50% -format '%[fx:mean*w*h]' info:)
[ "${INK%.*}" -ge 100 ] || { echo "FAIL: bundled-font fixture did not paint text"; exit 1; }

cat > "$CONFIG_FILE" <<'EOF'
[appearance]
theme = "dracula"
font = "Scribe Deliberately Missing Font"
cursor_blink = false
EOF
wait_for_log 'terminal font is unavailable' 10 \
    || { echo "FAIL: missing configured font did not take the safe fallback"; exit 1; }
sleep 0.6
import -window "$WID" /output/04-missing-font.png
convert /output/04-missing-font.png -crop 350x72+8+40 +repage /output/missing-font-grid.png
compare -metric AE /output/bundled-font-grid.png /output/missing-font-grid.png null: 2>/output/font-diff.txt \
    || { echo "FAIL: missing-family fallback changed terminal text pixels"; exit 1; }
[ "$(pgrep -f '(^|/)scribe-client$' | head -1)" = "$PID_BEFORE" ] \
    || { echo "FAIL: font reload restarted the client"; exit 1; }
echo "PASS: bundled primary and missing-family fallback paint identical text without host fonts"
