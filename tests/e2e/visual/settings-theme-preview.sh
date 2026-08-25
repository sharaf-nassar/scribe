#!/usr/bin/env bash
# Capture the Colors page: palette first, then the preset menu open, then the
# same menu with the pointer resting on a row so the hover preview is visible.
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
eval "$(xdotool getwindowgeometry --shell "$WIN")"
echo "settings window ${WIDTH}x${HEIGHT} at ${X},${Y}"

shot() {
    sleep 0.7
    scrot -o "$OUT/$1"
    convert "$OUT/$1" -crop "${WIDTH}x${HEIGHT}+${X}+${Y}" +repage "$OUT/$1"
}

# Colors page through the search field, as a user would reach it.
xdotool key --window "$WIN" ctrl+k
sleep 0.4
xdotool type --window "$WIN" --delay 40 "colors"
sleep 0.8
xdotool key --window "$WIN" Return
sleep 0.6
xdotool key --window "$WIN" Escape
sleep 0.8
shot theme-01-colors-page.png

# Open the Preset menu by clicking its trigger, then hover successive rows.
# The trigger sits on the first row under the Theme heading; find it by moving
# to the value column at that row's height.
PRESET_X=$(( X + 900 ))
PRESET_Y=$(( Y + 489 ))
xdotool mousemove "$PRESET_X" "$PRESET_Y" click 1
sleep 1.0
shot theme-02-menu-open.png

# Rest the pointer on a menu row a little below the trigger: the palette grid
# and terminal preview above should repaint to that preset.
xdotool mousemove $(( PRESET_X - 110 )) $(( PRESET_Y + 120 ))
sleep 1.2
shot theme-03-hover-preview.png

xdotool mousemove $(( PRESET_X - 110 )) $(( PRESET_Y + 210 ))
sleep 1.2
shot theme-04-hover-preview-2.png

echo "PASS: captured theme preview states"
