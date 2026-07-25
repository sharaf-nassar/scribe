#!/bin/bash
# Scripted E2E: the in-app entry points open the real settings window.
#
# The settings window was complete for the whole rebuild and unreachable from
# inside the terminal: `KeyAction::OpenSettings` hit a swallow arm in
# `dispatch_key_action`, so the only way to the surface was the
# `scribe-client-gpui --settings` CLI flag. That took every setting, and the
# update/trust actions that live on those pages, out of the running app.
#
# A headless test can prove the dispatcher arm exists; it cannot prove a second
# top-level window really mapped on screen. So every phase drives the running
# client through XTEST and asserts on the X server: a window titled "Scribe
# Settings" — the exact `TitlebarOptions` title `open_settings_window` sets —
# has to appear, be painted, and never be opened twice.
#
#   * Ctrl+, (the `keybindings.settings` default) maps the settings window and
#     paints it, and the client logs "opened the settings window";
#   * pressing it again from the terminal window raises the SAME window: the
#     count stays at one and the client logs "focused the open settings window",
#     the line only the retained `WindowHandle` path writes;
#   * the palette row "Open Settings" reaches the same handler, proving the
#     palette and the chord converge (`key_action_for_automation`);
#   * clicking the titlebar gear does too — its `TitlebarEvent::OpenSettings`
#     had no subscriber at all before this bead.
#
# Input is driven through XTEST (plain `xdotool key` / `click`, no `--window`).
# GPUI reads pointer and keyboard through XInput2 and ignores the synthetic
# events `xdotool --window` sends with XSendEvent, so window-targeted input
# would leave the client untouched while the script still "passed".
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

# Lit pixels the settings window must paint. It draws an eleven-row sidebar and
# a page of labelled controls in near-white on a dark surface, far more than
# this; an unpainted or blank window would fall under it.
SETTINGS_INK_MIN="${SETTINGS_INK_MIN:-500}"

# Titlebar geometry, from crates/scribe-client-gpui/src/titlebar.rs: the band is
# TITLEBAR_HEIGHT=34 tall, and the gear is a 34px icon button sitting directly
# left of the three 40px window controls (the equalize button next to it is
# hidden while the window has no split). A titlebar layout change turns into a
# failing phase here rather than a silent miss.
TITLEBAR_HEIGHT=34
WINDOW_CONTROLS_WIDTH=120
GEAR_WIDTH=34

TERM_X=0
TERM_Y=0
TERM_W=0
TERM_H=0

# The terminal window only. `--name Scribe` is an unanchored regex and would
# also match "Scribe Settings", which is the very window these phases count.
list_terminal_windows() {
    xdotool search --name '^Scribe$' 2>/dev/null || true
}

list_settings_windows() {
    xdotool search --name '^Scribe Settings$' 2>/dev/null || true
}

count_settings_windows() {
    list_settings_windows | grep -c . || true
}

# Wait until the number of mapped settings windows reaches `want`.
wait_for_settings_windows() {
    local want="$1" timeout_secs="${2:-15}" started
    started=$(date +%s)
    while true; do
        [ "$(count_settings_windows)" -eq "$want" ] && return 0
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Focus the terminal window and cache its on-screen geometry, so titlebar
# coordinates can be replayed as absolute XTEST pointer moves.
#
# Re-focusing before every phase is load-bearing: the settings window takes the
# X11 focus when it opens, and the client's own active-window guard suppresses
# keystrokes aimed at a window that is not `_NET_ACTIVE_WINDOW`.
focus_terminal() {
    local wid
    wid=$(list_terminal_windows | tail -1)
    if [ -z "$wid" ]; then
        fail "FAIL: no Scribe terminal window found"
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.5
    # `xwininfo`, not `xdotool getwindowgeometry`: openbox reparents the window
    # into a decorated frame, and xdotool reports that frame's origin. The gear
    # sits 17px below the *client* origin, so a frame-relative click would land
    # in the window manager's own title bar instead of the button.
    local info
    info=$(xwininfo -id "$wid")
    TERM_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    TERM_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    TERM_W=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    TERM_H=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Count lit pixels inside the settings window's on-screen rectangle.
settings_ink() {
    local wid info x y w h value
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && { printf '0'; return; }
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    w=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    h=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    value=$(convert "$1" -crop "${w}x${h}+${x}+${y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.4
}

# Click at a point relative to the terminal window's origin, through XTEST.
click_terminal_at() {
    xdotool mousemove "$(( TERM_X + $1 ))" "$(( TERM_Y + $2 ))"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    exit 1
}

# ── Phase 0: one terminal window, no settings window ──────────────
focus_terminal
if [ "$(count_settings_windows)" -ne 0 ]; then
    fail "PHASE 0 FAIL: a settings window was already open before any entry point ran"
fi
shot /output/00-terminal-only.png
echo "PHASE 0 PASS: terminal window up (${TERM_W}x${TERM_H}), no settings window"

# ── Phase 1: the settings chord opens the window ──────────────────
# Ctrl+, is `keybindings.settings`' Linux default. The client line is written
# only by `open_or_focus_settings`; the mapped window is what proves the handler
# actually reached `open_settings_window` and the platform put it on screen.
OPENS_BEFORE=$(count_log "opened the settings window")
send_keys ctrl+comma
if ! wait_for_settings_windows 1 15; then
    fail "PHASE 1 FAIL: ctrl+, mapped no settings window (still swallowed?)"
fi
if ! wait_for_log_growth "opened the settings window" "$OPENS_BEFORE" 10; then
    fail "PHASE 1 FAIL: the chord never reached open_or_focus_settings"
fi
shot /output/01-settings-open.png
INK=$(settings_ink /output/01-settings-open.png)
if [ "$INK" -lt "$SETTINGS_INK_MIN" ]; then
    fail "PHASE 1 FAIL: the settings window painted $INK px (min $SETTINGS_INK_MIN)"
fi
echo "PHASE 1 PASS: ctrl+, opened the settings window (ink $INK)"

# ── Phase 2: the chord again raises it, never duplicates it ───────
# The retained `WindowHandle` is the deduplication. A second open would leave
# two windows titled "Scribe Settings" both writing config.toml.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
send_keys ctrl+comma
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 2 FAIL: the second chord did not raise the open settings window"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 2 FAIL: expected exactly one settings window, found $COUNT"
fi
shot /output/02-settings-refocused.png
echo "PHASE 2 PASS: the chord raised the same window (still $COUNT settings window)"

# ── Phase 3: the palette row lands on the same handler ────────────
# "Open Settings" is the palette's first row. It lowers onto the same
# `KeyAction::OpenSettings` the chord produces, so a routed row raises the open
# window exactly like phase 2 did.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
send_keys ctrl+shift+p
type_text "Open Settings"
shot /output/03-palette-open-settings.png
send_keys Return
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 3 FAIL: the palette 'Open Settings' row never reached the handler"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 3 FAIL: the palette row opened a duplicate window ($COUNT total)"
fi
echo "PHASE 3 PASS: the palette row reached the settings handler"

# ── Phase 4: the titlebar gear is wired too ───────────────────────
# The gear has been painted since the titlebar landed, but its
# `TitlebarEvent::OpenSettings` had no subscriber, so clicking it did nothing.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
GEAR_X=$(( TERM_W - WINDOW_CONTROLS_WIDTH - GEAR_WIDTH / 2 ))
GEAR_Y=$(( TITLEBAR_HEIGHT / 2 ))
echo "clicking the gear at window-relative +${GEAR_X}+${GEAR_Y} (client origin ${TERM_X},${TERM_Y})"
click_terminal_at "$GEAR_X" "$GEAR_Y"
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 4 FAIL: clicking the gear at +${GEAR_X}+${GEAR_Y} reached no handler"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 4 FAIL: the gear opened a duplicate window ($COUNT total)"
fi
shot /output/04-gear-click.png
echo "PHASE 4 PASS: the titlebar gear reached the settings handler"

echo ""
echo "PASS: visual settings-entry test"
echo "  Inspect screenshots in test-output/:"
echo "    00-terminal-only.png          — the client before any settings entry"
echo "    01-settings-open.png          — the settings window opened by ctrl+,"
echo "    02-settings-refocused.png     — the same window raised, not duplicated"
echo "    03-palette-open-settings.png  — palette filtered to 'Open Settings'"
echo "    04-gear-click.png             — after the titlebar gear click"
