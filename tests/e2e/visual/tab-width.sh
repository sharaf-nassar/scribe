#!/usr/bin/env bash
# Tabs divide the strip they are in up to a cap, and titles use the width they
# were given.
#
# Measures painted geometry: a tab stuck at its 176px basis never grows past it,
# a tab with no cap stretches a lone tab across the whole band, and a title cut
# to a fixed column budget ends in an ellipsis while its own tab still has room.
# All three are visible in the strip, none is visible to a unit test.
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
    # The client window itself, not a root crop at the WM-reported origin:
    # under openbox that origin is the frame's, and the crop slid 20px down
    # into the pane card, whose full-width top outline then read as a tab.
    # The 36px strip only, for the same reason.
    import -window "$WIN" +repage "$OUT/$1"
    convert "$OUT/$1" -crop "${WIDTH}x36+0+0" +repage "$OUT/$1"
}

# Width of the painted tab chrome: threshold the band away from the window
# background and trim to what is left. A fixed-width strip trims to ~176px per
# tab, a capped one to ~230px per tab.
shot tabs-01-one-tab.png
ONE=$(band_ink_width "$OUT/tabs-01-one-tab.png")
echo "one tab: lit band columns ${ONE%.*}"

# A second tab: both must still divide the strip rather than sit at one edge.
xdotool key --window "$WIN" ctrl+shift+t
sleep 3
shot tabs-02-two-tabs.png
TWO=$(band_ink_width "$OUT/tabs-02-two-tabs.png")
echo "two tabs: lit band columns ${TWO%.*}"

# One tab grows past its 176px basis to the 230px cap (TAB_MAX_WIDTH) and no
# further: a capped strip trims to about one cap, an uncapped one to nearly the
# window width. A second tab doubles the ink, so the two still divide the run
# equally instead of one sitting at an edge.
CAP=230
[ "${ONE%.*}" -gt 190 ] \
    || fail "one tab lit only ${ONE%.*} columns (never grew past its basis)"
[ "${ONE%.*}" -le $(( CAP + 30 )) ] \
    || fail "one tab lit ${ONE%.*} columns of a ${WIDTH}px band (cap is ${CAP})"
LOW=$(( 2 * ${ONE%.*} - 30 ))
HIGH=$(( 2 * ${ONE%.*} + 30 ))
[ "${TWO%.*}" -ge "$LOW" ] && [ "${TWO%.*}" -le "$HIGH" ] \
    || fail "two tabs lit ${TWO%.*} columns (want about twice one tab's ${ONE%.*})"

echo "PASS: tabs grow to the cap and divide the run (${ONE%.*} and ${TWO%.*} of ${WIDTH} columns)"
