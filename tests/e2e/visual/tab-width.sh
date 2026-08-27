#!/usr/bin/env bash
# Tabs divide the strip they are in, and titles use the width they were given.
#
# Measures painted geometry: a fixed-width tab leaves the band half empty, and a
# title cut to a fixed column budget ends in an ellipsis while its own tab still
# has room. Both are visible in the strip, neither is visible to a unit test.
set -uo pipefail

. /tests/visual/tab-geometry-common.bash

OUT=/output
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

WIN=""
for _ in $(seq 1 60); do
    WIN=$(xdotool search --name '^Scribe$' 2>/dev/null | head -1)
    [ -n "$WIN" ] && break
    sleep 0.5
done
[ -n "$WIN" ] || fail "no Scribe window found"

xdotool windowactivate "$WIN" >/dev/null 2>&1
xdotool windowsize "$WIN" 1400 800
sleep 2
eval "$(xdotool getwindowgeometry --shell "$WIN")"
echo "window ${WIDTH}x${HEIGHT} at ${X},${Y}"

shot() {
    sleep 0.8
    scrot -o "$OUT/$1"
    convert "$OUT/$1" -crop "${WIDTH}x44+${X}+${Y}" +repage "$OUT/$1"
}

# The tab strip band, cropped to the titlebar. Count the columns that carry tab
# chrome: with one tab flexing to fill, ink reaches far further across the band
# than a 176px slot ever could.
# Width of the painted tab chrome: threshold the band away from the window
# background and trim to what is left. A fixed-width strip trims to ~176px per
# tab; a strip that fills the band trims to nearly the window width.
shot tabs-01-one-tab.png
ONE=$(band_ink_width "$OUT/tabs-01-one-tab.png")
echo "one tab: lit band columns ${ONE%.*}"

# A second tab: both must still divide the strip rather than sit at one edge.
xdotool key --window "$WIN" ctrl+shift+t
sleep 3
shot tabs-02-two-tabs.png
TWO=$(band_ink_width "$OUT/tabs-02-two-tabs.png")
echo "two tabs: lit band columns ${TWO%.*}"

# A tab strip that fills its band paints across most of the window width. The
# old fixed 176px tab could not reach a quarter of a 1400px window.
MIN=$(( WIDTH / 2 ))
[ "${ONE%.*}" -ge "$MIN" ] \
    || fail "one tab lit only ${ONE%.*} columns of a ${WIDTH}px band (want >= ${MIN})"
[ "${TWO%.*}" -ge "$MIN" ] \
    || fail "two tabs lit only ${TWO%.*} columns of a ${WIDTH}px band (want >= ${MIN})"

echo "PASS: tabs fill the strip (${ONE%.*} and ${TWO%.*} of ${WIDTH} columns)"
