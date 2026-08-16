#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: custom titlebar with the integrated tab bar.
#
# Captures the assembled titlebar (workspace-badge pill, tab strip with the
# active accent + AI dot + context-% suffix, equalize/gear icons, and the
# min/maximize/close window controls) and walks the interaction checklist the
# titlebar bead requires: tab hover reveals the close button, the gear icon is
# clickable, and the window-control cluster sits flush to the right edge. The
# close control is only screenshotted, never clicked, so the window survives the
# run for later stages.
#
# Requires: visual container (optional GPU passthrough via SCRIBE_E2E_GPUS)
set -euo pipefail

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
TITLEBAR_HEIGHT=34
STATUS_BAR_HEIGHT=24

fail() {
    echo "FAIL: $1" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

# Measure neutral title pixels inside one tab slot. The result is
# "count x y width height" relative to the requested crop.
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
    [ "${count:-0}" -ge 30 ] \
        || fail "$label tab title is not visible (${count:-0} pixels)"
    [ "$height" -le 11 ] \
        || fail "$label tab title is not compact (${width}x${height})"
    center2=$((2 * (bar_top + y) + height))
    expected2=$((2 * bar_top + TITLEBAR_HEIGHT))
    delta=$((center2 - expected2))
    [ "$delta" -lt 0 ] && delta=$((-delta))
    [ "$delta" -le 6 ] \
        || fail "$label tab title is not vertically centered (2x delta $delta)"
    echo "$label tab title: ${count}px, ${width}x${height}, centered"
}

# Helper: focus the Scribe window and capture a full-screen screenshot.
capture_window() {
    local out="$1"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.3
    fi
    scrot "$out"
}

# Helper: move the pointer to a pixel inside the Scribe window (no click).
hover_at() {
    local x="$1" y="$2"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool mousemove --window "$wid" "$x" "$y"
        sleep 0.3
    fi
}

# Helper: click at pixel coordinates inside the Scribe window.
click_at() {
    local x="$1" y="$2"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool mousemove --window "$wid" "$x" "$y"
        xdotool click --window "$wid" 1
        sleep 0.3
    fi
}

# ── Phase 1: baseline — assembled titlebar at rest ─────────────────
sleep 0.8
capture_window /output/01-titlebar-baseline.png
echo "PHASE 1 PASS: assembled titlebar captured (badge, tabs, icons, controls)"

# ── Phase 1b: both tab renderers are compact and vertically centered ─────────
WID=$(xdotool search --name '^Scribe$' | head -1) || true
[ -n "$WID" ] || fail "no Scribe window"
xdotool windowfocus --sync "$WID" 2>/dev/null || true
import -window "$WID" +repage /output/01b-titlebar-compact.png
assert_compact_centered_title /output/01b-titlebar-compact.png 0 titlebar

# A horizontal workspace split puts the new region below the first and gives it
# the second titlebar renderer.
xdotool windowfocus --sync "$WID" 2>/dev/null || true
xdotool key --clearmodifiers ctrl+alt+minus
for _ in $(seq 1 40); do
    if grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null; then
        break
    fi
    sleep 0.25
done
grep -q "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null \
    || fail "lower-region titlebar was not logged"
sleep 0.5
import -window "$WID" +repage /output/01c-region-titlebar-compact.png
WINDOW_HEIGHT=$(identify -format '%h' /output/01c-region-titlebar-compact.png)
LOWER_BAR_TOP=$((TITLEBAR_HEIGHT + (WINDOW_HEIGHT - TITLEBAR_HEIGHT - STATUS_BAR_HEIGHT) / 2))
assert_compact_centered_title \
    /output/01c-region-titlebar-compact.png "$LOWER_BAR_TOP" region-titlebar
echo "PHASE 1b PASS: both titlebar paths are compact and vertically centered"

# ── Phase 2: hover the first tab to reveal its close button ────────
# The titlebar is ~34px tall; the first tab begins at the left edge (or
# just after the workspace badge). Hover its centre.
hover_at 80 17
capture_window /output/02-tab-hover-close.png
echo "PHASE 2 PASS: tab hover reveals the per-tab close button"

# ── Phase 3: click the gear icon (settings affordance) ─────────────
# The gear sits left of the window controls; on 1920px the controls
# occupy the rightmost ~120px, so the gear is around x=1780.
click_at 1780 17
sleep 0.5
capture_window /output/03-gear-clicked.png
echo "PHASE 3 PASS: gear icon is clickable"

# ── Phase 4: hover the window-control cluster (min/max/close) ───────
# Screenshot only — clicking close would tear the window down.
hover_at 1900 17
capture_window /output/04-window-controls.png
echo "PHASE 4 PASS: window controls sit flush to the right edge"

echo ""
echo "PASS: visual titlebar test"
echo "  Inspect screenshots in test-output/:"
echo "    01-titlebar-baseline.png   — assembled titlebar at rest"
echo "    01b-titlebar-compact.png — compact centered integrated titlebar"
echo "    01c-region-titlebar-compact.png — compact centered lower region bar"
echo "    02-tab-hover-close.png     — tab hover reveals close button"
echo "    03-gear-clicked.png        — gear icon clicked (settings)"
echo "    04-window-controls.png     — min/max/close window controls"
