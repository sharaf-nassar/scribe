#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the in-app entry points open the real settings window.
#
# The settings window was complete for the whole rebuild and unreachable from
# inside the terminal: `KeyAction::OpenSettings` hit a swallow arm in
# `dispatch_key_action`, so the only way to the surface was the
# `scribe-client --settings` CLI flag. That took every setting, and the
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
#   * the Workspaces page preserves an invalid root for correction, persists
#     the corrected root, removes it again, and reloads the server after both
#     valid mutations; the native chooser maps and cancellation leaves config
#     unchanged;
#   * Badge color 1 opens the native preset/custom palette, applies a preset,
#     rejects invalid exact text, saves canonical RGB live, and Reset restores
#     the default palette;
#   * pressing it again from the terminal window raises the SAME window: the
#     count stays at one and the client logs "focused the open settings window",
#     the line only the retained `WindowHandle` path writes;
#   * the palette row "Open Settings" reaches the same handler, proving the
#     palette and the chord converge (`key_action_for_automation`);
#   * clicking the status-bar gear does too — its `on_settings` handler
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
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
SERVER_RELOAD_PATTERN="config reloaded successfully via client request"

# Lit pixels the settings window must paint. It draws an eleven-row sidebar and
# a page of labelled controls in near-white on a dark surface, far more than
# this; an unpainted or blank window would fall under it.
SETTINGS_INK_MIN="${SETTINGS_INK_MIN:-500}"
# Ignore caret blink and subpixel noise: rejected input adds an inline message
# and grows its row, changing comfortably more than this many window pixels.
SETTINGS_CHANGE_MIN="${SETTINGS_CHANGE_MIN:-100}"

# Status bar geometry, from crates/scribe-client/src/status_bar.rs: the gear
# moved out of the retired titlebar button into the
# `window_chrome::STATUS_BAR_HEIGHT`=24px band at the window bottom, the last
# `flex_col` child of the window root, where `settings_gear` renders as the
# band row's last child inside its px_2 (8px) edge padding.
#
# The gear div's right edge is therefore `width - 8`, and its left edge is
# `width - 8 - (8 + glyph_advance)` — pl_2 plus the `⚙` advance. Clicking
# `width - 8 - GEAR_INSET` with an inset between 1 and 15 lands inside the
# div's hit rect for ANY glyph advance, so the phase does not depend on how
# the container's font measures U+2699. Half the 12px `text_xs` cell keeps
# the click on the painted glyph as well. A status-bar layout change turns
# into a failing phase here rather than a silent miss.
STATUS_BAR_HEIGHT=24
STATUS_BAR_EDGE_PADDING=8
GEAR_INSET=6
COMPACT_SETTINGS_WIDTH=1040
COMPACT_SETTINGS_HEIGHT=720
SETTINGS_STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
# A `workspace` search leaves Keybindings and Workspaces in separate groups.
# These offsets are the center of the second filtered row, derived from the
# settings window's 38px titlebar, the sidebar search block (12px top pad +
# 30px field), the 14px nav top pad, 28px group labels, 32px rows, and the
# 14px inter-group gap: 38+42+14+28+32+14+28+16 = 212.
FILTERED_WORKSPACES_X=140
FILTERED_WORKSPACES_Y=212
# Fixed 1040×720 composition: the color picker is right-aligned with its
# trigger and offset below it. These measured points land on its green hue
# swatch and custom palette.
COLOR_PICKER_HUE_X=825
COLOR_PICKER_HUE_Y=547
COLOR_PICKER_PALETTE_X=890
COLOR_PICKER_PALETTE_Y=609

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

list_chooser_windows() {
    xdotool search --name '^Open Folder$' 2>/dev/null || true
}

count_chooser_windows() {
    list_chooser_windows | grep -c . || true
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

wait_for_chooser_windows() {
    local want="$1" timeout_secs="${2:-15}" started
    started=$(date +%s)
    while true; do
        [ "$(count_chooser_windows)" -eq "$want" ] && return 0
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Focus the terminal window and cache its on-screen geometry, so window-chrome
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
    # into a decorated frame, and xdotool reports that frame's origin and size.
    # The gear sits in the last 24px of the *client* box, so a frame-relative
    # click would be offset by the decoration and miss the band entirely.
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

settings_size() {
    local wid info w h
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && { printf '0 0'; return; }
    info=$(xwininfo -id "$wid")
    w=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    h=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    printf '%s %s' "$w" "$h"
}

# Count pixels that changed inside the settings window only. A small threshold
# drops antialiasing noise while retaining the inline validation row.
settings_changed_pixels() {
    local before="$1" after="$2" wid info x y w h value
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && { printf '0'; return; }
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    w=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    h=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    value=$(convert "$before" "$after" -compose difference -composite \
        -crop "${w}x${h}+${x}+${y}" +repage -colorspace Gray -threshold 3% \
        -format "%[fx:mean*w*h]" info:)
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

# Search-filtered sidebar selection is the only intentional settings-window
# coordinate: it avoids ambiguous keyboard selection when Keybindings also
# matches `workspace`, then traversal uses semantic focus targets.
click_filtered_workspaces() {
    local wid info x y
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && fail "PHASE 2 FAIL: no settings window to select Workspaces"
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    xdotool mousemove "$(( x + FILTERED_WORKSPACES_X ))" \
        "$(( y + FILTERED_WORKSPACES_Y ))"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

click_settings_at() {
    local relative_x="$1" relative_y="$2" wid info x y
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && fail "PHASE 2 FAIL: no settings window for picker click"
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    xdotool mousemove "$(( x + relative_x ))" "$(( y + relative_y ))"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

assert_font_size() {
    python3 - "$CONFIG_FILE" "$1" <<'PY'
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as config_file:
        config = tomllib.load(config_file)
except FileNotFoundError:
    config = {}

actual = config.get("appearance", {}).get("font_size", 14)
expected = float(sys.argv[2])
if actual != expected:
    print(f"font size mismatch: expected {expected!r}, got {actual!r}")
    raise SystemExit(1)
PY
}

assert_workspace_roots() {
    python3 - "$CONFIG_FILE" "$@" <<'PY'
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as config_file:
        config = tomllib.load(config_file)
except FileNotFoundError:
    config = {}

actual = config.get("workspaces", {}).get("roots", [])
expected = sys.argv[2:]
if actual != expected:
    print(f"workspace roots mismatch: expected {expected!r}, got {actual!r}")
    raise SystemExit(1)
PY
}

open_workspace_root_chooser() {
    click_filtered_workspaces
    send_keys Down
    send_keys Down
    send_keys Return
    if ! wait_for_chooser_windows 1 15; then
        fail "PHASE 2 FAIL: Browse did not map the native Open Folder chooser"
    fi
}

# Read and assert the configured palette with Python's stdlib TOML/JSON support.
# A missing TOML key means the same eight defaults Rust deserializes.
workspace_badge_colors() {
    python3 - "$CONFIG_FILE" "$@" <<'PY'
import json
import sys
import tomllib

defaults = [
    "#a78bfa", "#38bdf8", "#6ee7b7", "#fb7185",
    "#fbbf24", "#a3e635", "#f472b6", "#22d3ee",
]
try:
    with open(sys.argv[1], "rb") as config_file:
        config = tomllib.load(config_file)
except FileNotFoundError:
    config = {}

colors = config.get("workspaces", {}).get("badge_colors", defaults)
command = sys.argv[2]
if command == "read":
    print(json.dumps(colors, separators=(",", ":")))
elif command == "default":
    print(json.dumps(defaults, separators=(",", ":")))
elif command == "first":
    print(colors[0])
elif command == "assert":
    expected = json.loads(sys.argv[3])
    if colors != expected:
        print(f"workspace badge colors mismatch: expected {expected!r}, got {colors!r}")
        raise SystemExit(1)
elif command == "assert-first":
    if not colors or colors[0] != sys.argv[3]:
        print(f"workspace badge color 1 mismatch: expected {sys.argv[3]!r}, got {colors!r}")
        raise SystemExit(1)
else:
    raise SystemExit(f"unknown badge color helper command: {command}")
PY
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

count_server_reloads() {
    grep -acF "$SERVER_RELOAD_PATTERN" "$SERVER_LOG" 2>/dev/null || true
}

wait_for_server_reload_growth() {
    local baseline="$1" timeout_secs="${2:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_server_reloads)
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
    echo "--- server log tail ---"
    tail -40 "$SERVER_LOG" || true
    exit 1
}

# ── Phase 0: one terminal window, no settings window ──────────────
focus_terminal
if [ "$(count_settings_windows)" -ne 0 ]; then
    fail "PHASE 0 FAIL: a settings window was already open before any entry point ran"
fi
if ! assert_workspace_roots; then
    fail "PHASE 0 FAIL: settings-entry fixture did not start with zero workspace roots"
fi
DEFAULT_BADGE_COLORS=$(workspace_badge_colors default)
ORIGINAL_BADGE_COLORS=$(workspace_badge_colors read)
if [ "$ORIGINAL_BADGE_COLORS" != "$DEFAULT_BADGE_COLORS" ]; then
    fail "PHASE 0 FAIL: settings-entry fixture did not start with the default badge palette"
fi
# Reproduce the old macOS settings app's physical-pixel Retina geometry. GPUI
# consumes logical coordinates, so accepting this as-is would clamp the window
# to almost the entire display instead of using the compact first-open size.
mkdir -p "$SETTINGS_STATE_DIR"
cat >"$SETTINGS_STATE_DIR/settings_state.toml" <<'TOML'
open = false

[geometry]
x = 296
y = 78
width = 3520
height = 2424
TOML
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
SETTINGS_SIZE=$(settings_size)
if [ "$SETTINGS_SIZE" != "$COMPACT_SETTINGS_WIDTH $COMPACT_SETTINGS_HEIGHT" ]; then
    fail "PHASE 1 FAIL: settings opened at $SETTINGS_SIZE, expected compact ${COMPACT_SETTINGS_WIDTH}x${COMPACT_SETTINGS_HEIGHT}"
fi
shot /output/01-settings-open.png
INK=$(settings_ink /output/01-settings-open.png)
if [ "$INK" -lt "$SETTINGS_INK_MIN" ]; then
    fail "PHASE 1 FAIL: the settings window painted $INK px (min $SETTINGS_INK_MIN)"
fi
echo "PHASE 1 PASS: ctrl+, opened the compact $SETTINGS_SIZE settings window (ink $INK)"

# @lat: [[test#Visual E2E Tests#In-app settings entry points#Numeric steppers accept exact entry]]
# ── Phase 1A: Font size accepts exact numeric entry ───────────────
# Appearance is page 0. Eleven tabs reach Font family; one more reaches Font
# size. Enter opens exact entry rather than stepping, so this is the original
# regression flow that saved 16.0 on the old step-only stepper.
for _ in {1..12}; do
    send_keys Tab
done
send_keys Return
send_keys ctrl+a
type_text "23"
RELOADS_BEFORE=$(count_server_reloads)
send_keys Return
if ! assert_font_size 23; then
    fail "PHASE 1A FAIL: exact Font size entry did not persist 23"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 1A FAIL: exact Font size entry triggered no server live reload"
fi
shot /output/01a-font-size-23.png

# Leaving exact entry commits the valid value, then the same focused stepper
# reopens for the requested final value. This keeps blur on the native input
# path rather than creating a second numeric editor.
send_keys ctrl+a
type_text "22"
RELOADS_BEFORE=$(count_server_reloads)
send_keys Tab
if ! assert_font_size 22; then
    fail "PHASE 1A FAIL: blurring Font size did not persist 22"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 1A FAIL: blurred Font size triggered no server live reload"
fi
send_keys Up
send_keys Return
send_keys ctrl+a
type_text "23"
send_keys Return
if ! assert_font_size 23; then
    fail "PHASE 1A FAIL: reopening Font size did not restore 23"
fi

# Rejected text remains visible with its inline error and cannot write config.
send_keys ctrl+a
type_text "abc"
shot /output/01a-font-size-nonnumeric-before.png
send_keys Return
if ! assert_font_size 23; then
    fail "PHASE 1A FAIL: non-numeric Font size changed config"
fi
shot /output/01a-font-size-nonnumeric-error.png
CHANGED=$(settings_changed_pixels \
    /output/01a-font-size-nonnumeric-before.png /output/01a-font-size-nonnumeric-error.png)
if [ "$CHANGED" -lt "$SETTINGS_CHANGE_MIN" ]; then
    fail "PHASE 1A FAIL: non-numeric Font size showed no inline error"
fi

send_keys ctrl+a
type_text "49"
send_keys Return
if ! assert_font_size 23; then
    fail "PHASE 1A FAIL: out-of-range Font size changed config"
fi
shot /output/01a-font-size-out-of-range.png

# Escape drops the rejected text and restores the saved number on screen.
send_keys Escape
shot /output/01a-font-size-escape.png
if ! assert_font_size 23; then
    fail "PHASE 1A FAIL: Escape changed config"
fi
CHANGED=$(settings_changed_pixels \
    /output/01a-font-size-out-of-range.png /output/01a-font-size-escape.png)
if [ "$CHANGED" -lt "$SETTINGS_CHANGE_MIN" ]; then
    fail "PHASE 1A FAIL: Escape left the rejected Font size on screen"
fi

# Escape closed exact entry, so the stepper answers Left again — the closed
# arrow adjustment the typed field deliberately swallows while it is open.
send_keys Left
if ! assert_font_size 22; then
    fail "PHASE 1A FAIL: Left did not step the closed Font size stepper"
fi

# Put the fixture default back so later phases see the terminal they expect.
send_keys Return
send_keys ctrl+a
type_text "14"
send_keys Return
send_keys Escape
if ! assert_font_size 14; then
    fail "PHASE 1A FAIL: Font size did not return to the fixture default"
fi
echo "PHASE 1A PASS: exact and blurred Font size saved, rejected values stayed editable"

# @lat: [[test#Visual E2E Tests#In-app settings entry points#Workspace roots edit and apply live]]
# ── Phase 2: workspace roots reject, persist, and remove live ─────
# `workspace` deliberately matches both Keybindings and Workspaces. Select the
# visible Workspaces row, then let the window's semantic focus order reach the
# input. With no roots it is next; with one root, its Remove action is next.
send_keys ctrl+k
shot /output/02-search-focused-empty.png
type_text "workspace"
click_filtered_workspaces
send_keys Down

# Rejection must retain the text. Capture before and after Return so the oracle
# isolates the inline validation change rather than accepting typed glyphs.
type_text "~"
shot /output/02-workspace-bare-tilde.png
send_keys Return
if ! assert_workspace_roots; then
    fail "PHASE 2 FAIL: invalid bare ~ changed workspace roots"
fi
shot /output/02-workspace-invalid.png
CHANGED=$(settings_changed_pixels \
    /output/02-workspace-bare-tilde.png /output/02-workspace-invalid.png)
if [ "$CHANGED" -lt "$SETTINGS_CHANGE_MIN" ]; then
    fail "PHASE 2 FAIL: invalid root changed only $CHANGED settings-window pixels (min $SETTINGS_CHANGE_MIN)"
fi

# Continue in the same input: exact persistence proves the rejected `~` was
# preserved rather than cleared and replaced by the suffix.
RELOADS_BEFORE=$(count_server_reloads)
type_text "/scribe-e2e-workspaces"
send_keys Return
if ! assert_workspace_roots "~/scribe-e2e-workspaces"; then
    fail "PHASE 2 FAIL: corrected workspace root was not persisted exactly once"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: adding a workspace root triggered no server live reload"
fi
shot /output/02-workspace-added.png

# Re-selecting Workspaces resets the semantic origin. With one configured root,
# one Down lands on its first Remove action rather than the input.
click_filtered_workspaces
send_keys Down
RELOADS_BEFORE=$(count_server_reloads)
send_keys Return
if ! assert_workspace_roots; then
    fail "PHASE 2 FAIL: keyboard Remove left a configured workspace root"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: removing a workspace root triggered no server live reload"
fi
shot /output/02-workspace-removed.png

# Browse uses GPUI's native directory prompt. The Linux implementation reaches
# the real XDG portal, whose GTK backend maps this Open Folder window. Escape
# must return cancellation without mutating config or reloading the server.
open_workspace_root_chooser
shot /output/02-workspace-chooser.png
RELOADS_BEFORE=$(count_server_reloads)
send_keys Escape
if ! wait_for_chooser_windows 0 15; then
    fail "PHASE 2 FAIL: Escape did not cancel the directory chooser"
fi
sleep 1
if ! assert_workspace_roots; then
    fail "PHASE 2 FAIL: cancelling the chooser changed workspace roots"
fi
if [ "$(count_server_reloads)" -ne "$RELOADS_BEFORE" ]; then
    fail "PHASE 2 FAIL: cancelling the chooser triggered a server reload"
fi
shot /output/02-workspace-chooser-cancelled.png
echo "PHASE 2 PASS: typed add/remove persisted live; native chooser mapped and cancellation was a no-op ($CHANGED changed px)"

# @lat: [[test#Visual E2E Tests#In-app settings entry points#Workspace badge colors edit and reset live]]
# The filtered Workspaces page has two matching nav rows. From the clicked
# Workspaces row, focus order is root input, Browse, Add, eight colors, then Reset.
click_filtered_workspaces
send_keys Down
send_keys Down
send_keys Down
send_keys Down

send_keys Return
shot /output/02-badge-color-picker.png
RELOADS_BEFORE=$(count_server_reloads)
send_keys Right
if ! workspace_badge_colors assert-first "#000000"; then
    fail "PHASE 2 FAIL: Badge color 1 preset did not apply live"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: Badge color 1 preset triggered no server live reload"
fi
shot /output/02-badge-color-preset.png

RELOADS_BEFORE=$(count_server_reloads)
click_settings_at "$COLOR_PICKER_HUE_X" "$COLOR_PICKER_HUE_Y"
click_settings_at "$COLOR_PICKER_PALETTE_X" "$COLOR_PICKER_PALETTE_Y"
CUSTOM_BADGE_COLOR=$(workspace_badge_colors first)
if [ "$CUSTOM_BADGE_COLOR" = "#000000" ] \
    || ! printf '%s\n' "$CUSTOM_BADGE_COLOR" | grep -Eq '^#[0-9a-f]{6}$'; then
    fail "PHASE 2 FAIL: custom palette produced invalid color $CUSTOM_BADGE_COLOR"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: custom palette selection triggered no server live reload"
fi
shot /output/02-badge-color-custom.png

# Tab enters the palette's exact-value field without adding another semantic
# row stop. Validation remains available for hand-authored hex and ansi:N.
send_keys Tab
send_keys ctrl+a
type_text "not-a-color"
send_keys Return
if ! workspace_badge_colors assert-first "$CUSTOM_BADGE_COLOR"; then
    fail "PHASE 2 FAIL: invalid badge color changed the configured palette"
fi
shot /output/02-badge-color-invalid.png

# Escape abandons exact entry and closes the picker; reopen it before entering
# the corrected value so keyboard focus cannot remain trapped in hidden chrome.
send_keys Escape
shot /output/02-badge-color-escape.png
click_filtered_workspaces
send_keys Down
send_keys Down
send_keys Down
send_keys Down
send_keys Return
send_keys Tab
send_keys ctrl+a
type_text "112233"
RELOADS_BEFORE=$(count_server_reloads)
send_keys Return
if ! workspace_badge_colors assert-first "#112233"; then
    fail "PHASE 2 FAIL: Badge color 1 was not canonicalized to #112233"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: saving Badge color 1 triggered no server live reload"
fi
shot /output/02-badge-color-saved.png
send_keys Tab
shot /output/02-badge-color-tab-closed.png

# Exactly 12 Down stops: root input + Browse + Add + 8 colors + Reset.
click_filtered_workspaces
for _ in {1..12}; do
    send_keys Down
done
RELOADS_BEFORE=$(count_server_reloads)
send_keys Return
if ! workspace_badge_colors assert "$DEFAULT_BADGE_COLORS"; then
    fail "PHASE 2 FAIL: Reset did not restore the default badge palette"
fi
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: resetting badge colors triggered no server live reload"
fi
shot /output/02-badge-colors-reset.png
echo "PHASE 2 PASS: Badge color 1 applied preset and custom colors, canonicalized #112233, then Reset restored defaults"

# ── Phase 3: the chord again raises it, never duplicates it ───────
# The retained `WindowHandle` is the deduplication. A second open would leave
# two windows titled "Scribe Settings" both writing config.toml.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
send_keys ctrl+comma
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 3 FAIL: the second chord did not raise the open settings window"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 3 FAIL: expected exactly one settings window, found $COUNT"
fi
shot /output/03-settings-refocused.png
echo "PHASE 3 PASS: the chord raised the same window (still $COUNT settings window)"

# ── Phase 4: the palette row lands on the same handler ────────────
# "Open Settings" is the palette's first row. It lowers onto the same
# `KeyAction::OpenSettings` the chord produces, so a routed row raises the open
# window exactly like phase 3 did.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
send_keys ctrl+shift+p
type_text "Open Settings"
shot /output/04-palette-open-settings.png
send_keys Return
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 4 FAIL: the palette 'Open Settings' row never reached the handler"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 4 FAIL: the palette row opened a duplicate window ($COUNT total)"
fi
echo "PHASE 4 PASS: the palette row reached the settings handler"

# ── Phase 5: the status-bar gear is wired too ─────────────────────
# The settings gear lives at the far right of the bottom status bar
# (`settings_gear` in status_bar.rs); its `on_settings` handler must reach
# the same open-or-focus path as the chord and the palette row.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
GEAR_X=$(( TERM_W - STATUS_BAR_EDGE_PADDING - GEAR_INSET ))
GEAR_Y=$(( TERM_H - STATUS_BAR_HEIGHT / 2 ))
echo "clicking the gear at window-relative +${GEAR_X}+${GEAR_Y} (client origin ${TERM_X},${TERM_Y})"
click_terminal_at "$GEAR_X" "$GEAR_Y"
if ! wait_for_log_growth "focused the open settings window" "$FOCUSES_BEFORE" 10; then
    fail "PHASE 5 FAIL: clicking the gear at +${GEAR_X}+${GEAR_Y} reached no handler"
fi
COUNT=$(count_settings_windows)
if [ "$COUNT" -ne 1 ]; then
    fail "PHASE 5 FAIL: the gear opened a duplicate window ($COUNT total)"
fi
shot /output/05-gear-click.png
echo "PHASE 5 PASS: the status-bar gear reached the settings handler"

# ── Phase 6: the Pi integration row is reachable and operable by keyboard ─
# Pi's provider toggle is an ordinary AI-page row, so the only thing the
# settings surface owes it is that a keyboard-only user can find it and flip
# it. `pi integration` matches exactly one page and exactly one control, so
# the filtered focus ring is two stops long: the page, then the toggle.
# Whichever of the two the first Down lands on, activating the page rewinds
# the ring to its start, so the second pass always reaches the toggle.
focus_settings() {
    local wid
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && fail "PHASE 6 FAIL: no settings window to drive by keyboard"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.5
}

pi_integration_enabled() {
    python3 - "$CONFIG_FILE" <<'PY'
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as config_file:
        config = tomllib.load(config_file)
except FileNotFoundError:
    config = {}

print(str(config.get("terminal", {}).get("pi_integration", True)).lower())
PY
}

focus_settings
if [ "$(pi_integration_enabled)" != "true" ]; then
    fail "PHASE 6 FAIL: Pi integration did not start enabled by default"
fi
# The search input is append-only and Ctrl+K only focuses it, so phase 2's
# query has to be dismissed first — Escape is the deliberate clear.
send_keys ctrl+k
send_keys Escape
send_keys ctrl+k
type_text "pi integration"
shot /output/06-pi-integration-filtered.png
# Tab is the documented way out of the search field; Down then walks the ring.
send_keys Tab
send_keys Down
send_keys Return
if [ "$(pi_integration_enabled)" = "true" ]; then
    send_keys Down
    send_keys Return
fi
if [ "$(pi_integration_enabled)" != "false" ]; then
    fail "PHASE 6 FAIL: keyboard activation did not turn Pi integration off"
fi
shot /output/06-pi-integration-off.png
send_keys Return
if [ "$(pi_integration_enabled)" != "true" ]; then
    fail "PHASE 6 FAIL: the focused Pi row did not turn back on from the keyboard"
fi
shot /output/06-pi-integration-on.png
echo "PHASE 6 PASS: the Pi integration row was found and toggled with the keyboard alone"

echo ""
echo "PASS: visual settings-entry test"
echo "  Inspect screenshots in test-output/:"
echo "    00-terminal-only.png          — the client before any settings entry"
echo "    01-settings-open.png          — the settings window opened by ctrl+,"
echo "    01a-font-size-23.png          — exact Font size persisted as a number"
echo "    01a-font-size-nonnumeric-error.png — rejected numeric text retained inline"
echo "    01a-font-size-out-of-range.png — rejected bounds retained inline"
echo "    01a-font-size-escape.png      — Escape restored the saved number"
echo "    02-search-focused-empty.png   — focused empty search without visual placeholder"
echo "    02-workspace-bare-tilde.png   — rejected root before inline validation"
echo "    02-workspace-invalid.png      — rejected root retained with inline error"
echo "    02-workspace-added.png        — corrected root rendered from config"
echo "    02-workspace-removed.png      — root removed through keyboard traversal"
echo "    02-workspace-chooser.png      — native portal directory chooser"
echo "    02-workspace-chooser-cancelled.png — cancellation left roots unchanged"
echo "    02-badge-color-picker.png     — preset and custom palette opened"
echo "    02-badge-color-preset.png     — keyboard preset applied live"
echo "    02-badge-color-custom.png     — pointer hue and palette applied live"
echo "    02-badge-color-invalid.png    — invalid RGB retained with inline error"
echo "    02-badge-color-escape.png     — Escape closed exact entry and picker"
echo "    02-badge-color-saved.png      — canonical #112233 editor and swatch"
echo "    02-badge-color-tab-closed.png — Tab left exact entry and closed picker"
echo "    02-badge-colors-reset.png     — eight default badge colors restored"
echo "    03-settings-refocused.png     — the same window raised, not duplicated"
echo "    04-palette-open-settings.png  — palette filtered to 'Open Settings'"
echo "    05-gear-click.png             — after the status-bar gear click"
echo "    06-pi-integration-filtered.png — search filtered to the Pi row"
echo "    06-pi-integration-off.png     — Pi integration toggled off by keyboard"
echo "    06-pi-integration-on.png      — the same row toggled back on"
