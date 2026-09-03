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
# Two things in the current chrome defeat a plain "bright near-gray ink" crop:
#
#   * The focused tab's accent tick is a NEUTRAL muted gray whenever no project
#     has named the workspace, so it passes the near-gray title filter. Its
#     full-width 2px rule stretched the reported title box from 10px to 17px.
#     Rows that span almost the whole window are therefore dropped: a title
#     never does, a rule always does.
#   * A window holding more than one region grows a workspace pill at each
#     bar's left. Its label carries descenders the tab title does not, so a
#     crop anchored at x=8 measured the pill and read 4.5px low. The scan
#     starts after the pill's filled card when one is present.
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

mid = bar_h // 2
edge = px[(width - 4, mid)]
pill = px[(2, mid)]
left = 0
if dist(pill, edge) > 6:
    run = 0
    for x in range(width):
        if dist(px[(x, mid)], pill) <= 6:
            left, run = x, 0
        else:
            run += 1
            if run >= 8 and left:
                break
    left += 6

lo, hi = left, min(left + crop_w, width)
rows = {}
for y in range(bar_h):
    ink = [x for x in range(lo, hi)
           if px[(x, y)][0] > 102 and abs(px[(x, y)][0] - px[(x, y)][1]) < 8
           and abs(px[(x, y)][1] - px[(x, y)][2]) < 8]
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
