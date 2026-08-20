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

# shellcheck source=tests/e2e/visual/agent-visual-common.bash
. /tests/visual/agent-visual-common.bash

HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
DELTA_MIN="${AI_INDICATOR_DELTA_MIN:-60}"

magenta_pixels() {
    convert "$1" -alpha off \
        -fx '(r > g + 0.15 && b > g + 0.15 && r > 0.2 && b > 0.2) ? 1 : 0' \
        -format '%[fx:mean*w*h]' info: | tail -1
}

WID=$(find_scribe_window)
[ -n "$WID" ] || fail "no Scribe window"
focus_scribe_window "$WID"
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

# Pi is an ordinary provider to every shared surface: the same hook channel,
# the same tab dot, the same pane border, and the same per-provider enable
# toggle. These phases send a Pi state where the phases above sent a Claude
# one, so a Pi-specific chrome path would show up as a missing repaint here.
hook() {
    SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper "$@"
}

capture_chrome() {
    local name="$1"
    shot "/output/ai-indicator-$name.png"
    convert "/output/ai-indicator-$name.png" -crop 400x34+0+0 +repage \
        "/output/ai-indicator-tab-$name.png"
    convert "/output/ai-indicator-$name.png" -crop "2x${GRID_H}+0+34" +repage \
        "/output/ai-indicator-border-$name.png"
}

wait_for_reload() {
    local baseline="$1" seen
    for _ in $(seq 1 50); do
        seen=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
        [ "${seen:-0}" -gt "$baseline" ] && { sleep 0.8; return 0; }
        sleep 0.2
    done
    return 1
}

# @lat: [[test#Test Harness#Visual E2E Tests#AI indicator paints provider state]]
# ── Pi phase 1: a cleared pane is the baseline both Pi phases measure ────────
hook --provider=claude_code --event=state_cleared
sleep 1
capture_chrome 03-pi-cleared

# ── Pi phase 2: an enabled Pi state paints the shared chrome ─────────────────
hook --provider=pi --event=state_changed --state=processing
sleep 1
capture_chrome 04-pi-processing
PI_TAB_DELTA=$(delta /output/ai-indicator-tab-03-pi-cleared.png /output/ai-indicator-tab-04-pi-processing.png)
PI_BORDER_DELTA=$(delta /output/ai-indicator-border-03-pi-cleared.png /output/ai-indicator-border-04-pi-processing.png)
[ "${PI_TAB_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "an enabled Pi state did not tint the tab ($PI_TAB_DELTA changed pixels)"
[ "${PI_BORDER_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "an enabled Pi state did not paint the pane border ($PI_BORDER_DELTA changed pixels)"
echo "PASS: Pi painted the shared tab tint ($PI_TAB_DELTA px) and pane border ($PI_BORDER_DELTA px)"

# ── Pi phase 3: the provider toggle hides that chrome live ───────────────────
# `terminal.pi_integration = false` is the same provider gate Claude Code and
# Codex have. Turning it off must strip the visible chrome from the state the
# client is already tracking, without another hook edge.
RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
cat >>"$CONFIG_FILE" <<'EOF'

[terminal]
pi_integration = false
EOF
wait_for_reload "${RELOADS_BEFORE:-0}" || fail "the client never hot-reloaded the Pi provider toggle"
capture_chrome 05-pi-disabled
PI_OFF_BORDER_DELTA=$(delta /output/ai-indicator-border-04-pi-processing.png /output/ai-indicator-border-05-pi-disabled.png)
PI_OFF_RESIDUE=$(delta /output/ai-indicator-border-03-pi-cleared.png /output/ai-indicator-border-05-pi-disabled.png)
[ "${PI_OFF_BORDER_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "disabling Pi integration left the pane border painted ($PI_OFF_BORDER_DELTA changed pixels)"
[ "${PI_OFF_RESIDUE:-0}" -lt "$DELTA_MIN" ] \
    || fail "a disabled Pi provider still painted chrome ($PI_OFF_RESIDUE pixels off the cleared pane)"

# ── Pi phase 4: re-enabling brings the tracked state back ────────────────────
RELOADS_BEFORE=$(grep -cF "config hot-reloaded" "$CLIENT_LOG" 2>/dev/null || true)
sed -i 's/^pi_integration = false$/pi_integration = true/' "$CONFIG_FILE"
wait_for_reload "${RELOADS_BEFORE:-0}" || fail "the client never hot-reloaded the Pi provider re-enable"
capture_chrome 06-pi-reenabled
PI_BACK_DELTA=$(delta /output/ai-indicator-border-05-pi-disabled.png /output/ai-indicator-border-06-pi-reenabled.png)
[ "${PI_BACK_DELTA:-0}" -ge "$DELTA_MIN" ] \
    || fail "re-enabling Pi integration did not repaint the tracked state ($PI_BACK_DELTA changed pixels)"
echo "PASS: the Pi provider toggle hid ($PI_OFF_BORDER_DELTA px) and restored ($PI_BACK_DELTA px) the shared chrome live"
