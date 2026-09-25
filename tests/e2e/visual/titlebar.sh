#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# Blocking visual smoke for both production titlebar renderers. Settings entry,
# tab interaction, and window lifecycle have stronger dedicated suites.
set -euo pipefail

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
TITLEBAR_HEIGHT=36
STATUS_BAR_HEIGHT=36
# Columns of one bar searched for the tab title, starting after any workspace
# pill. One tab title is far narrower; this only has to exclude the next tab.
TITLE_SCAN_WIDTH=100

fail() {
    echo "FAIL: $1" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

# Locate the tab title inside one bar, as `count x y width height` with `y`
# relative to the bar's top.
#
# Three things in the current chrome defeat a plain "bright near-gray ink" crop:
#
#   * The open tab's card wears its pane's outline and a keyboard mark in a
#     NEUTRAL muted gray whenever no project has named the workspace, so both
#     pass the near-gray title filter. The outline's vertical rules are found
#     first (columns lit down most of the band), everything in their colour is
#     dropped, and rows spanning almost the whole card (its top edge) go too.
#   * A window holding more than one region grows a workspace pill at each
#     bar's left. Its label carries descenders the tab title does not, so a
#     crop anchored at the bar's edge measured the pill and read 4.5px low.
#   * Tabs cap at 230px, so a lone tab's title sits in the first fraction of
#     a wide bar and a fixed crop could miss it entirely.
#   The scan is therefore bounded by the open tab's own two outline rules,
#   which the pill lies left of and the bare run right of.
title_geometry() {
    local image="$1" bar_top="$2"
    python3 - "$image" "$bar_top" "$TITLEBAR_HEIGHT" "$TITLE_SCAN_WIDTH" <<'PY'
import subprocess, sys

image, bar_top, bar_h, crop_w = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
raw = subprocess.run(
    ["convert", image, "-crop", f"x{bar_h}+0+{bar_top}", "+repage", "txt:-"],
    capture_output=True, text=True, check=True,
).stdout

px = {}
width = 0
for line in raw.splitlines():
    if not line or line[0] == "#":
        continue
    pos, rest = line.split(":", 1)
    x, y = (int(v) for v in pos.split(","))
    body = rest.split("(", 1)[1].split(")", 1)[0]
    r, g, b = (int(float(v)) for v in body.split(",")[:3])
    px[(x, y)] = (r, g, b)
    width = max(width, x + 1)

def dist(a, b):
    return max(abs(a[i] - b[i]) for i in range(3))

def near_gray(c):
    return c[0] > 102 and abs(c[0] - c[1]) < 8 and abs(c[1] - c[2]) < 8

# The open tab's card wears its pane's outline: two one-pixel vertical rules
# the full card tall, in a muted gray that passes the title filter. They are
# chrome, not title ink, but they also bound the title: the open tab is the
# first card in the bar, so its title lies between the first two rules. A
# workspace pill to the left and a bare run to the right both fall outside.
span = {}
for y in range(bar_h):
    for x in range(width):
        if near_gray(px[(x, y)]):
            span[x] = span.get(x, 0) + 1
rules = sorted(x for x, n in span.items() if n > 14)
if len(rules) < 2:
    print("0 0 0 0 0")
    raise SystemExit
lo, hi = rules[0] + 1, min(rules[1], rules[0] + 1 + crop_w)
outline = {px[(x, y)] for x in rules[:2] for y in range(bar_h) if near_gray(px[(x, y)])}

rows = {}
for y in range(bar_h):
    # Drop the outline's own colour too: its top edge, corner, and the
    # keyboard mark share it, while the title is far brighter.
    ink = [x for x in range(lo, hi)
           if near_gray(px[(x, y)]) and all(dist(px[(x, y)], c) > 6 for c in outline)]
    if ink and len(ink) < 0.9 * (hi - lo):
        rows[y] = ink

if not rows:
    print("0 0 0 0 0")
    raise SystemExit

top, bottom = min(rows), max(rows)
cols = [x for ink in rows.values() for x in ink]
count = sum(len(ink) for ink in rows.values())
print(count, min(cols) - lo, top, max(cols) - min(cols) + 1, bottom - top + 1)
PY
}

# Capture once the client has actually painted. A debug-profile client can
# still be blank at a fixed sleep, and an empty capture fails the title
# assertions as if the titlebar had regressed.
capture_painted() {
    local target="$1" tries=0 ink
    while [ "$tries" -lt 40 ]; do
        import -window "$WID" +repage "$target"
        ink=$(convert "$target" -crop "300x${TITLEBAR_HEIGHT}+0+0" +repage \
            -colorspace Gray -threshold 25% -format '%[fx:mean*w*h]' info:)
        [ "${ink%.*}" -ge 30 ] && return 0
        tries=$(( tries + 1 ))
        sleep 0.25
    done
    fail "titlebar never painted (last capture had ${ink%.*} lit pixels)"
}

assert_compact_centered_title() {
    local image="$1" bar_top="$2" label="$3"
    local count x y width height center2 expected2 delta
    read -r count x y width height <<<"$(title_geometry "$image" "$bar_top")"
    [ "${count:-0}" -ge 30 ] || fail "$label tab title is not visible (${count:-0} pixels)"
    [ "$height" -le 11 ] || fail "$label tab title is not compact (${width}x${height})"
    center2=$((2 * (bar_top + y) + height))
    expected2=$((2 * bar_top + TITLEBAR_HEIGHT))
    delta=$((center2 - expected2))
    [ "$delta" -lt 0 ] && delta=$((-delta))
    [ "$delta" -le 6 ] \
        || fail "$label tab title is not vertically centered (2x delta $delta)"
}

sleep 0.8
WID=$(xdotool search --name '^Scribe$' | head -1) || true
[ -n "$WID" ] || fail "no Scribe window"
xdotool windowfocus --sync "$WID" 2>/dev/null || true
capture_painted /output/titlebar-compact.png
assert_compact_centered_title /output/titlebar-compact.png 0 titlebar

# A pane split changes the active tab's content tree, never the titlebar's tab
# count. Keep this smoke in the titlebar path because an accidental strip insert
# is visible here before any lower-region bar is created.
tabs_before=$(grep -c "opened a new tab" "$CLIENT_LOG" 2>/dev/null || true)
adopts_before=$(grep -c "pane adopted a session" "$CLIENT_LOG" 2>/dev/null || true)
xdotool key --clearmodifiers ctrl+shift+backslash
for _ in $(seq 1 40); do
    adopts_now=$(grep -c "pane adopted a session" "$CLIENT_LOG" 2>/dev/null || true)
    [ "$adopts_now" -gt "$adopts_before" ] && break
    sleep 0.25
done
[ "${adopts_now:-0}" -gt "$adopts_before" ] || fail "pane split never adopted its session"
sleep 0.5
[ "$(grep -c "opened a new tab" "$CLIENT_LOG" 2>/dev/null || true)" -eq "$tabs_before" ] \
    || fail "pane split inserted a titlebar tab"

xdotool key --clearmodifiers ctrl+alt+minus
for _ in $(seq 1 40); do
    grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null && break
    sleep 0.25
done
grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null \
    || fail "lower-region titlebar was not logged"
sleep 0.5
capture_painted /output/region-titlebar-compact.png
WINDOW_HEIGHT=$(identify -format '%h' /output/region-titlebar-compact.png)
LOWER_BAR_TOP=$((TITLEBAR_HEIGHT + (WINDOW_HEIGHT - TITLEBAR_HEIGHT - STATUS_BAR_HEIGHT) / 2))
assert_compact_centered_title \
    /output/region-titlebar-compact.png "$LOWER_BAR_TOP" region-titlebar

echo "PASS: both titlebar paths are compact and vertically centered"
