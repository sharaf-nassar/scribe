#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E: a real provider hook paints the GPUI AI chrome.
#
# The hook helper writes to the real server socket. The server broadcasts the
# resulting AiStateChanged frame to the shared pane, so every changed pixel
# below proves the actual hook → IPC reader → tracker → GPUI paint path.
#
# Requires the shared-pane visual rig (`SCRIBE_SHARED_PANE=1`).
set -e

HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
DELTA_MIN="${AI_INDICATOR_DELTA_MIN:-60}"

fail() {
    echo "FAIL: $1" >&2
    tail -40 "${SCRIBE_CLIENT_LOG:-/output/client.log}" 2>/dev/null >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

shot() {
    import -window "$WID" +repage "$1"
}

delta() {
    local changed
    changed=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${changed%%.*}"
}

magenta_pixels() {
    convert "$1" -alpha off \
        -fx '(r > g + 0.15 && b > g + 0.15 && r > 0.2 && b > 0.2) ? 1 : 0' \
        -format '%[fx:mean*w*h]' info: | tail -1
}

WID=$(find_window)
[ -n "$WID" ] || fail "no Scribe window"
xdotool windowactivate --sync "$WID" 2>/dev/null \
    || xdotool windowfocus --sync "$WID" 2>/dev/null || true
sleep 0.5

shot /output/ai-indicator-00-before.png
read -r WIN_W WIN_H <<<"$(identify -format '%w %h' /output/ai-indicator-00-before.png)"
GRID_H=$((WIN_H - 34))
convert /output/ai-indicator-00-before.png -crop 400x34+0+0 +repage \
    /output/ai-indicator-tab-before.png
convert /output/ai-indicator-00-before.png -crop "2x${GRID_H}+0+34" +repage \
    /output/ai-indicator-border-before.png

SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper \
    --provider=claude_code --event=state_changed --state=processing
sleep 1

shot /output/ai-indicator-01-processing.png
convert /output/ai-indicator-01-processing.png -crop 400x34+0+0 +repage \
    /output/ai-indicator-tab-processing.png
convert /output/ai-indicator-01-processing.png -crop "2x${GRID_H}+0+34" +repage \
    /output/ai-indicator-border-processing.png

TAB_DELTA=$(delta /output/ai-indicator-tab-before.png /output/ai-indicator-tab-processing.png)
BORDER_DELTA=$(delta /output/ai-indicator-border-before.png /output/ai-indicator-border-processing.png)
[ "${TAB_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "provider hook did not tint the tab ($TAB_DELTA changed pixels)"
[ "${BORDER_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "provider hook did not paint the pane border ($BORDER_DELTA changed pixels)"
echo "PASS: hook painted tab tint ($TAB_DELTA px) and pane border ($BORDER_DELTA px)"

# @lat: [[test#Test Harness#Visual E2E Tests#AI indicator paints provider state]]
RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
cat >>"$CONFIG_FILE" <<'EOF'

[terminal.ai_states.processing]
color = "#ff00ff"
EOF

# Bounded to 50 polls (10 seconds): the watcher must apply the saved state
# colour to the already-active indicator without waiting for another hook edge.
for _ in $(seq 1 50); do
    RELOADS_AFTER=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
    [ "$RELOADS_AFTER" -gt "${RELOADS_BEFORE:-0}" ] && break
    sleep 0.2
done
[ "${RELOADS_AFTER:-0}" -gt "${RELOADS_BEFORE:-0}" ] \
    || fail "client did not hot-reload the configured AI state color"
sleep 0.5

shot /output/ai-indicator-02-configured-color.png
convert /output/ai-indicator-02-configured-color.png -crop 400x34+0+0 +repage \
    /output/ai-indicator-tab-configured-color.png
convert /output/ai-indicator-02-configured-color.png -crop "2x${GRID_H}+0+34" +repage \
    /output/ai-indicator-border-configured-color.png
TAB_COLOR_DELTA=$(delta \
    /output/ai-indicator-tab-processing.png \
    /output/ai-indicator-tab-configured-color.png)
BORDER_COLOR_DELTA=$(delta \
    /output/ai-indicator-border-processing.png \
    /output/ai-indicator-border-configured-color.png)
TAB_MAGENTA=$(magenta_pixels /output/ai-indicator-tab-configured-color.png)
BORDER_MAGENTA=$(magenta_pixels /output/ai-indicator-border-configured-color.png)
[ "${TAB_COLOR_DELTA:-0}" -ge 12 ] && [ "${TAB_MAGENTA:-0}" -ge 12 ] \
    || fail "hot-reloaded signal color did not repaint the active tab dot ($TAB_COLOR_DELTA changed, $TAB_MAGENTA magenta pixels)"
[ "${BORDER_COLOR_DELTA:-0}" -ge "$DELTA_MIN" ] \
    && [ "${BORDER_MAGENTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "hot-reloaded signal color did not repaint the pane border ($BORDER_COLOR_DELTA changed, $BORDER_MAGENTA magenta pixels)"
echo "PASS: configured signal color repainted tab ($TAB_COLOR_DELTA px) and border ($BORDER_COLOR_DELTA px)"
