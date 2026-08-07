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
#   * Badge color 1 rejects invalid RGB text without changing config, saves a
#     canonical RGB value live, and Reset restores the default palette;
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

# Titlebar geometry, from crates/scribe-client/src/titlebar.rs: the band is
# TITLEBAR_HEIGHT=34 tall, and the gear is a 34px icon button sitting directly
# left of the three 40px window controls (the equalize button next to it is
# hidden while the window has no split). A titlebar layout change turns into a
# failing phase here rather than a silent miss.
TITLEBAR_HEIGHT=34
WINDOW_CONTROLS_WIDTH=120
GEAR_WIDTH=34
COMPACT_SETTINGS_WIDTH=1040
COMPACT_SETTINGS_HEIGHT=720
SETTINGS_STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
# A `workspace` search leaves Keybindings and Workspaces in separate groups.
# These offsets are the center of the second filtered row, derived from the
# settings window's 32px titlebar, 18px nav top pad, 46px group headings, 44px
# rows, and 13px inter-group seam.
FILTERED_WORKSPACES_X=140
FILTERED_WORKSPACES_Y=221

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
ORIGINAL_BADGE_COLOR_1=$(workspace_badge_colors first)
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

send_keys ctrl+a
type_text "not-a-color"
send_keys Return
if ! workspace_badge_colors assert "$ORIGINAL_BADGE_COLORS"; then
    fail "PHASE 2 FAIL: invalid badge color changed the configured palette"
fi
shot /output/02-badge-color-invalid.png

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
echo "PHASE 2 PASS: Badge color 1 changed from $ORIGINAL_BADGE_COLOR_1 to #112233, then Reset restored defaults"

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

# ── Phase 5: the titlebar gear is wired too ───────────────────────
# The gear has been painted since the titlebar landed, but its
# `TitlebarEvent::OpenSettings` had no subscriber, so clicking it did nothing.
FOCUSES_BEFORE=$(count_log "focused the open settings window")
focus_terminal
GEAR_X=$(( TERM_W - WINDOW_CONTROLS_WIDTH - GEAR_WIDTH / 2 ))
GEAR_Y=$(( TITLEBAR_HEIGHT / 2 ))
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
echo "PHASE 5 PASS: the titlebar gear reached the settings handler"

echo ""
echo "PASS: visual settings-entry test"
echo "  Inspect screenshots in test-output/:"
echo "    00-terminal-only.png          — the client before any settings entry"
echo "    01-settings-open.png          — the settings window opened by ctrl+,"
echo "    02-search-focused-empty.png   — focused empty search without visual placeholder"
echo "    02-workspace-bare-tilde.png   — rejected root before inline validation"
echo "    02-workspace-invalid.png      — rejected root retained with inline error"
echo "    02-workspace-added.png        — corrected root rendered from config"
echo "    02-workspace-removed.png      — root removed through keyboard traversal"
echo "    02-workspace-chooser.png      — native portal directory chooser"
echo "    02-workspace-chooser-cancelled.png — cancellation left roots unchanged"
echo "    02-badge-color-invalid.png    — invalid RGB retained with inline error"
echo "    02-badge-color-saved.png      — canonical #112233 editor and swatch"
echo "    02-badge-colors-reset.png     — eight default badge colors restored"
echo "    03-settings-refocused.png     — the same window raised, not duplicated"
echo "    04-palette-open-settings.png  — palette filtered to 'Open Settings'"
echo "    05-gear-click.png             — after the titlebar gear click"
