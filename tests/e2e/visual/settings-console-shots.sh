#!/usr/bin/env bash
# Capture the settings window on each page it redesigns, so the Console port is
# judged against real pixels rather than the HTML mock it was ported from.
#
# Not a gate: it asserts only that the window painted, then writes one PNG per
# page for visual review. Every page change goes through the window's own
# keyboard traversal (Ctrl+K search, then Enter), never a pixel offset.
set -uo pipefail

OUT=/output
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

WIN=""
for _ in $(seq 1 60); do
    WIN=$(xdotool search --name '^Scribe Settings$' 2>/dev/null | head -1)
    [ -n "$WIN" ] && break
    sleep 0.5
done
[ -n "$WIN" ] || fail "no Scribe Settings window found"

xdotool windowactivate "$WIN" >/dev/null 2>&1
xdotool windowraise "$WIN" >/dev/null 2>&1
sleep 1.5

info=$(xdotool getwindowgeometry --shell "$WIN")
eval "$info"
echo "settings window ${WIDTH}x${HEIGHT} at ${X},${Y}"

shot() {
    sleep 0.6
    scrot -o "$OUT/$1"
    convert "$OUT/$1" -crop "${WIDTH}x${HEIGHT}+${X}+${Y}" +repage "$OUT/$1"
}

ink() {
    convert "$1" -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:
}

# Page 1: Appearance, the default.
shot console-01-appearance.png
PAINTED=$(ink "$OUT/console-01-appearance.png")
[ "${PAINTED%.*}" -gt 2000 ] || fail "window painted only ${PAINTED} px of ink"

# Reach a page by name through the search field, exactly as a user would.
goto() {
    xdotool key --window "$WIN" ctrl+k
    sleep 0.4
    xdotool type --window "$WIN" --delay 40 "$1"
    sleep 0.8
    xdotool key --window "$WIN" Return
    sleep 0.8
    xdotool key --window "$WIN" Escape
    sleep 0.5
}

goto colors
shot console-02-colors.png

goto keybindings
shot console-03-keybindings.png

goto updates
shot console-04-updates.png

goto environment
shot console-05-environment.png

echo "PASS: captured 5 settings pages"
