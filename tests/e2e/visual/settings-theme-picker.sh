#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual-settings-theme-picker)." >&2; exit 99; }
# Visual E2E: the Colors theme picker previews, filters, applies, and resets
# through the running GPUI settings window.
set -euo pipefail

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
RELOAD_PATTERN="config hot-reloaded"
FILTER_CHANGE_MIN="${FILTER_CHANGE_MIN:-500}"

WIN=""
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -40 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

list_terminal_windows() {
    xdotool search --name '^Scribe$' 2>/dev/null || true
}

list_settings_windows() {
    xdotool search --name '^Scribe Settings$' 2>/dev/null || true
}

wait_for_settings_window() {
    local started
    started=$(date +%s)
    while true; do
        [ "$(list_settings_windows | grep -c . || true)" -eq 1 ] && return 0
        [ $(( "$(date +%s)" - started )) -ge 15 ] && return 1
        sleep 0.3
    done
}

focus_terminal() {
    local wid
    wid=$(list_terminal_windows | tail -1)
    [ -n "$wid" ] || fail "PHASE 0 FAIL: no Scribe terminal window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.5
}

focus_settings() {
    local info
    WIN=$(list_settings_windows | tail -1)
    [ -n "$WIN" ] || fail "no Scribe Settings window found"
    xdotool windowactivate --sync "$WIN" 2>/dev/null \
        || xdotool windowfocus --sync "$WIN" 2>/dev/null || true
    sleep 0.5
    info=$(xwininfo -id "$WIN")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    WIN_H=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
}

shot() {
    local out="$1" raw="/output/settings-theme-picker-screen.png"
    sleep 0.3
    scrot -o "$raw"
    convert "$raw" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$out"
    echo "captured $out"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.4
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.5
}

press_down() {
    local count="$1" i
    for ((i = 0; i < count; i++)); do
        xdotool key --clearmodifiers Down
        sleep 0.15
    done
    sleep 0.3
}

click_settings_at() {
    xdotool mousemove "$(( WIN_X + $1 ))" "$(( WIN_Y + $2 ))"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

mouse_down_settings_at() {
    xdotool mousemove "$(( WIN_X + $1 ))" "$(( WIN_Y + $2 ))"
    sleep 0.3
    xdotool mousedown 1
}

mouse_up_settings() {
    xdotool mouseup 1
    sleep 0.6
}

changed_pixels() {
    local before="$1" after="$2" value
    value=$(compare -metric AE "$before" "$after" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

changed_region_pixels() {
    local before="$1" after="$2" x="$3" y="$4" width="$5" height="$6" value
    convert "$before" -crop "${width}x${height}+${x}+${y}" +repage \
        /tmp/settings-theme-picker-before.png
    convert "$after" -crop "${width}x${height}+${x}+${y}" +repage \
        /tmp/settings-theme-picker-after.png
    value=$(compare -metric AE /tmp/settings-theme-picker-before.png \
        /tmp/settings-theme-picker-after.png null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

color_count() {
    local png="$1" color="$2" x="$3" y="$4" width="$5" height="$6" value
    value=$(convert "$png" -crop "${width}x${height}+${x}+${y}" +repage \
        -fuzz 3% -fill black +opaque "$color" -fill white -opaque "$color" \
        -colorspace Gray -format '%[fx:mean*w*h]' info:)
    printf '%s' "${value%.*}"
}

# Find one 8x8 solid block of a known swatch color and return its center in
# window-relative coordinates. Text antialiasing cannot satisfy the block.
find_solid_color() {
    local png="$1" color="$2" x="$3" y="$4" width="$5" height="$6"
    python3 - "$png" "$color" "$x" "$y" "$width" "$height" <<'PY'
import re
import subprocess
import sys

png, color = sys.argv[1:3]
x, y, width, height = map(int, sys.argv[3:])
target = tuple(bytes.fromhex(color.removeprefix("#")))
text = subprocess.check_output(
    ["convert", png, "-crop", f"{width}x{height}+{x}+{y}", "+repage", "txt:-"],
    text=True,
)
pixels = set()
for line in text.splitlines():
    match = re.match(r"(\d+),(\d+): \((\d+),(\d+),(\d+)", line)
    if not match:
        continue
    px, py, red, green, blue = map(int, match.groups())
    if max(abs(channel - expected) for channel, expected in zip((red, green, blue), target)) <= 5:
        pixels.add((px, py))

for px, py in sorted(pixels, key=lambda point: (point[1], point[0])):
    if all((px + dx, py + dy) in pixels for dx in range(8) for dy in range(8)):
        print(px + x + 4, py + y + 4)
        raise SystemExit(0)
raise SystemExit(1)
PY
}

find_dracula_strip() {
    python3 - "$1" <<'PY'
import re
import subprocess
import sys

expected = [
    (0x28, 0x2A, 0x36), (0xF8, 0xF8, 0xF2),
    (0x21, 0x22, 0x2C), (0xFF, 0x55, 0x55),
    (0x50, 0xFA, 0x7B), (0xF1, 0xFA, 0x8C),
    (0xBD, 0x93, 0xF9), (0xFF, 0x79, 0xC6),
    (0x8B, 0xE9, 0xFD), (0xF8, 0xF8, 0xF2),
]
text = subprocess.check_output(
    ["convert", sys.argv[1], "-crop", "300x460+720+100", "+repage", "txt:-"],
    text=True,
)
pixels = {}
for line in text.splitlines():
    match = re.match(r"(\d+),(\d+): \((\d+),(\d+),(\d+)", line)
    if match:
        px, py, red, green, blue = map(int, match.groups())
        pixels[px, py] = red, green, blue

for py in range(460):
    for px in range(300 - 79):
        actual = [pixels.get((px + index * 8, py)) for index in range(10)]
        if all(
            color is not None
            and max(abs(channel - wanted) for channel, wanted in zip(color, target)) <= 5
            for color, target in zip(actual, expected)
        ):
            print(px + 720, py + 100)
            raise SystemExit(0)
raise SystemExit("Dracula preview strip pixels were not found")
PY
}

count_reloads() {
    grep -acF "$RELOAD_PATTERN" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_reload_count() {
    local want="$1" started
    started=$(date +%s)
    while true; do
        [ "$(count_reloads)" -ge "$want" ] && return 0
        [ $(( "$(date +%s)" - started )) -ge 15 ] && return 1
        sleep 0.3
    done
}

wait_for_client_log() {
    local pattern="$1" started
    started=$(date +%s)
    while true; do
        grep -qF "$pattern" "$CLIENT_LOG" 2>/dev/null && return 0
        [ $(( "$(date +%s)" - started )) -ge 10 ] && return 1
        sleep 0.3
    done
}

assert_preset() {
    python3 - "$CONFIG_FILE" "$1" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as config_file:
    config = tomllib.load(config_file)
actual = config.get("appearance", {}).get("theme")
if actual != sys.argv[2]:
    raise SystemExit(f"appearance.theme mismatch: expected {sys.argv[2]!r}, got {actual!r}")
PY
}

assert_prompt_override() {
    python3 - "$CONFIG_FILE" "$1" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as config_file:
    config = tomllib.load(config_file)
actual = config.get("appearance", {}).get("prompt_bar_first_row_bg")
if actual != sys.argv[2]:
    raise SystemExit(f"prompt_bar_first_row_bg mismatch: expected {sys.argv[2]!r}, got {actual!r}")
PY
}

assert_prompt_override_absent() {
    python3 - "$CONFIG_FILE" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as config_file:
    config = tomllib.load(config_file)
appearance = config.get("appearance", {})
if "prompt_bar_first_row_bg" in appearance:
    raise SystemExit(
        f"prompt_bar_first_row_bg should be absent, got {appearance['prompt_bar_first_row_bg']!r}"
    )
PY
}

assert_assignment_once() {
    python3 - "$CONFIG_FILE" "$1" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
count = len(re.findall(rf"^\s*{re.escape(sys.argv[2])}\s*=", text, re.MULTILINE))
if count != 1:
    raise SystemExit(f"{sys.argv[2]} assignment count: expected 1, got {count}")
PY
}

# Phase 0: open the real settings window from the running client.
wait_for_client_log "X11 active-window guard enabled" \
    || fail "PHASE 0 FAIL: client focus guard never became ready"
focus_terminal
[ "$(list_settings_windows | grep -c . || true)" -eq 0 ] \
    || fail "PHASE 0 FAIL: a settings window was already open"
send_keys ctrl+comma
wait_for_settings_window || fail "PHASE 0 FAIL: ctrl+, opened no settings window"
focus_settings
[ "$WIN_W $WIN_H" = "1040 720" ] \
    || fail "PHASE 0 FAIL: settings geometry is ${WIN_W}x${WIN_H}, expected 1040x720"
echo "PHASE 0 PASS: compact settings window opened"

# @lat: [[test#Visual E2E Tests#Settings theme picker#Preset preview and filter]]
# Phase 1: reach Colors and open Preset through semantic keyboard traversal.
send_keys Down
send_keys Return
press_down 10
send_keys Return
shot /output/theme-picker-01-menu.png
type_text "dracula"
shot /output/theme-picker-02-filtered.png
FILTER_CHANGED=$(changed_pixels \
    /output/theme-picker-01-menu.png /output/theme-picker-02-filtered.png)
[ "$FILTER_CHANGED" -ge "$FILTER_CHANGE_MIN" ] \
    || fail "PHASE 1 FAIL: filtering repainted $FILTER_CHANGED px (min $FILTER_CHANGE_MIN)"
read -r DRACULA_X DRACULA_Y <<<"$(find_dracula_strip \
    /output/theme-picker-02-filtered.png)" \
    || fail "PHASE 1 FAIL: filtered Dracula row has no known preview strip"
echo "PHASE 1 PASS: preset menu narrowed to the pixel-verified Dracula row"

# @lat: [[test#Visual E2E Tests#Settings theme picker#Held preset press stays inside the menu]]
# Phase 2: keep the pointer down across a painted frame. The deferred menu
# must stay open, block the color selector below it, and commit only on release.
RELOADS_BEFORE=$(count_reloads)
mouse_down_settings_at "$DRACULA_X" "$DRACULA_Y"
sleep 0.2
shot /output/theme-picker-03-held-dracula.png
mouse_up_settings
find_dracula_strip /output/theme-picker-03-held-dracula.png >/dev/null \
    || fail "PHASE 2 FAIL: Dracula mousedown dismissed the deferred menu"
UNDERLAY_CHANGED=$(changed_region_pixels \
    /output/theme-picker-02-filtered.png /output/theme-picker-03-held-dracula.png \
    320 "$(( DRACULA_Y - 4 ))" 350 8)
[ "$UNDERLAY_CHANGED" -le 20 ] \
    || fail "PHASE 2 FAIL: covered Colors row repainted $UNDERLAY_CHANGED px while held"
wait_for_reload_count "$(( RELOADS_BEFORE + 1 ))" \
    || fail "PHASE 2 FAIL: held Dracula click produced no client hot reload"
sleep 1
RELOADS_AFTER=$(count_reloads)
[ "$RELOADS_AFTER" -eq "$(( RELOADS_BEFORE + 1 ))" ] \
    || fail "PHASE 2 FAIL: held Dracula click produced $(( RELOADS_AFTER - RELOADS_BEFORE )) hot reloads, expected 1"
assert_preset dracula || fail "PHASE 2 FAIL: held click did not save Dracula"
assert_assignment_once theme \
    || fail "PHASE 2 FAIL: appearance.theme was not serialized exactly once"
xdotool mousemove "$(( WIN_X + 400 ))" "$(( WIN_Y + 100 ))"
shot /output/theme-picker-04-held-applied.png
if find_solid_color /output/theme-picker-04-held-applied.png \
    '#ef4444' 650 100 350 500 >/dev/null; then
    fail "PHASE 2 FAIL: covered color selector opened after the held Dracula click"
fi
echo "PHASE 2 PASS: held Dracula click stayed in the menu, persisted once, and opened no covered selector"

# @lat: [[test#Visual E2E Tests#Settings theme picker#Keyboard apply persists once]]
# Phase 3: move away from Dracula, then choose it again with Down and Enter.
send_keys Return
type_text "custom"
RELOADS_BEFORE=$(count_reloads)
send_keys Down
send_keys Return
wait_for_reload_count "$(( RELOADS_BEFORE + 1 ))" \
    || fail "PHASE 3 FAIL: keyboard setup did not save Custom"
send_keys Return
type_text "dracula"
RELOADS_BEFORE=$(count_reloads)
send_keys Down
send_keys Return
wait_for_reload_count "$(( RELOADS_BEFORE + 1 ))" \
    || fail "PHASE 3 FAIL: preset apply produced no client hot reload"
sleep 1
RELOADS_AFTER=$(count_reloads)
[ "$RELOADS_AFTER" -eq "$(( RELOADS_BEFORE + 1 ))" ] \
    || fail "PHASE 3 FAIL: preset apply produced $(( RELOADS_AFTER - RELOADS_BEFORE )) hot reloads, expected 1"
assert_preset dracula || fail "PHASE 3 FAIL: Dracula was not saved to appearance.theme"
shot /output/theme-picker-05-applied.png
AMBER_CLOSED=$(color_count /output/theme-picker-05-applied.png '#f5b83a' 720 100 300 460)
send_keys Return
shot /output/theme-picker-06-selected.png
AMBER_OPEN=$(color_count /output/theme-picker-06-selected.png '#f5b83a' 720 100 300 460)
[ "$AMBER_OPEN" -gt "$(( AMBER_CLOSED + 10 ))" ] \
    || fail "PHASE 3 FAIL: selected menu chrome added only $(( AMBER_OPEN - AMBER_CLOSED )) amber px"
send_keys Escape
echo "PHASE 3 PASS: keyboard applied Dracula, saved TOML, painted selected chrome, and hot-reloaded once"

# @lat: [[test#Visual E2E Tests#Settings theme picker#Derived swatch keeps its trigger visible]]
# Phase 4: filter to First Row, then prove its unset Dracula-derived swatch
# survives underneath the open color menu.
send_keys ctrl+k
type_text "First Row"
shot /output/theme-picker-07-derived-swatch.png
read -r SWATCH_X SWATCH_Y <<<"$(find_solid_color \
    /output/theme-picker-07-derived-swatch.png '#232531' 650 100 350 500)" \
    || fail "PHASE 4 FAIL: unset First Row swatch is not Dracula-derived #232531"
click_settings_at "$SWATCH_X" "$SWATCH_Y"
shot /output/theme-picker-08-color-menu.png
VISIBLE_SWATCH=$(color_count /output/theme-picker-08-color-menu.png '#232531' \
    "$(( SWATCH_X - 4 ))" "$(( SWATCH_Y - 4 ))" 8 8)
[ "$VISIBLE_SWATCH" -ge 56 ] \
    || fail "PHASE 4 FAIL: open color menu obscured its trigger swatch ($VISIBLE_SWATCH/64 px)"
echo "PHASE 4 PASS: unset derived swatch is #232531 and its trigger remains visible"

# @lat: [[test#Visual E2E Tests#Settings theme picker#Held color preset persists once]]
# @lat: [[test#Visual E2E Tests#Settings theme picker#Reset omits the override key]]
# Phase 5: hold one color preset through a frame, then close and reset it.
read -r RED_X RED_Y <<<"$(find_solid_color \
    /output/theme-picker-08-color-menu.png '#ef4444' 650 100 350 500)" \
    || fail "PHASE 5 FAIL: red color preset was not visible"
RELOADS_BEFORE=$(count_reloads)
mouse_down_settings_at "$RED_X" "$RED_Y"
sleep 0.2
shot /output/theme-picker-09-held-color-preset.png
mouse_up_settings
find_solid_color /output/theme-picker-09-held-color-preset.png \
    '#ef4444' 650 100 350 500 >/dev/null \
    || fail "PHASE 5 FAIL: color-preset mousedown dismissed the deferred menu"
wait_for_reload_count "$(( RELOADS_BEFORE + 1 ))" \
    || fail "PHASE 5 FAIL: held color preset produced no client hot reload"
sleep 1
RELOADS_AFTER=$(count_reloads)
[ "$RELOADS_AFTER" -eq "$(( RELOADS_BEFORE + 1 ))" ] \
    || fail "PHASE 5 FAIL: held color preset produced $(( RELOADS_AFTER - RELOADS_BEFORE )) hot reloads, expected 1"
assert_prompt_override '#ef4444' \
    || fail "PHASE 5 FAIL: held preset did not write canonical #ef4444"
assert_assignment_once prompt_bar_first_row_bg \
    || fail "PHASE 5 FAIL: First Row override was not serialized exactly once"
send_keys Escape
send_keys Down
send_keys Return
assert_prompt_override_absent \
    || fail "PHASE 5 FAIL: Reset serialized an empty prompt-bar override"
shot /output/theme-picker-10-reset.png
echo "PHASE 5 PASS: held color preset persisted once and Reset removed the override"

echo "ALL PHASES PASS: held menu clicks, preview, filter, keyboard apply, swatch, and reset"
