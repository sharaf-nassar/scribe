#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# Blocking visual smoke for both production titlebar renderers. Settings entry,
# tab interaction, and window lifecycle have stronger dedicated suites.
set -euo pipefail

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
TITLEBAR_HEIGHT=34
STATUS_BAR_HEIGHT=24

fail() {
    echo "FAIL: $1" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

title_geometry() {
    local image="$1" crop="$2" mask="$3" trimmed="${3%.png}-trimmed.png"
    convert "$image" -crop "$crop" +repage -alpha off \
        -fx '(r > 0.4 && abs(r-g) < 0.03 && abs(g-b) < 0.03) ? 1 : 0' \
        "$mask" >/dev/null
    local count bounds
    count=$(identify -format '%[fx:mean*w*h]' "$mask")
    convert "$mask" -trim "$trimmed" >/dev/null 2>&1 || true
    bounds=$(identify -format '%[fx:page.x] %[fx:page.y] %w %h' "$trimmed" 2>/dev/null || true)
    printf '%s %s\n' "${count%.*}" "${bounds:-0 0 0 0}"
}

assert_compact_centered_title() {
    local image="$1" bar_top="$2" label="$3"
    local count x y width height center2 expected2 delta
    read -r count x y width height <<<"$(title_geometry \
        "$image" "100x${TITLEBAR_HEIGHT}+8+$bar_top" "/tmp/$label-title-mask.png")"
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
import -window "$WID" +repage /output/titlebar-compact.png
assert_compact_centered_title /output/titlebar-compact.png 0 titlebar

xdotool key --clearmodifiers ctrl+alt+minus
for _ in $(seq 1 40); do
    grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null && break
    sleep 0.25
done
grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null \
    || fail "lower-region titlebar was not logged"
sleep 0.5
import -window "$WID" +repage /output/region-titlebar-compact.png
WINDOW_HEIGHT=$(identify -format '%h' /output/region-titlebar-compact.png)
LOWER_BAR_TOP=$((TITLEBAR_HEIGHT + (WINDOW_HEIGHT - TITLEBAR_HEIGHT - STATUS_BAR_HEIGHT) / 2))
assert_compact_centered_title \
    /output/region-titlebar-compact.png "$LOWER_BAR_TOP" region-titlebar

echo "PASS: both titlebar paths are compact and vertically centered"
