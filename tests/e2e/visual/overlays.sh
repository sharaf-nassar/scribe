#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: the command palette, right-click context menu, and hover
# tooltip overlays of the GPUI client rebuild.
#
# Drives the live Scribe window and walks the interaction checklist the overlay
# bead requires: the command palette opens (Ctrl+Shift+P), filters as you type,
# and moves its selection; the right-click context menu opens at the cursor with
# its copy/open/hyperlink entries; and the tooltip demo (Ctrl+Shift+U) shows a
# head+tail-truncated URL clamped inside the viewport. Every overlay is drawn
# with rounded corners, a drop shadow, and hover/selected states.
#
# Requires: visual container (optional GPU passthrough via SCRIBE_E2E_GPUS), xdotool, scrot.
set -e

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | head -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | head -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool windowactivate --sync "$wid" 2>/dev/null \
            || xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.3
    fi
}

shot() {
    focus
    sleep 0.2
    scrot -o "$1"
    echo "captured $1"
}

send_keys() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool key --window "$wid" "$@"
        sleep 0.3
    fi
}

type_text() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool type --window "$wid" "$1"
        sleep 0.3
    fi
}

right_click_at() {
    local x="$1" y="$2" wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool mousemove --window "$wid" "$x" "$y"
        xdotool click 3
        sleep 0.3
    fi
}

# ── Phase 1: command palette opens and filters ────────────────────
sleep 0.8
focus
shot /output/00-overlays-baseline.png
send_keys ctrl+shift+p
shot /output/01-palette-open.png
DIFF=$(pixel_diff /output/00-overlays-baseline.png /output/01-palette-open.png)
[ "$DIFF" -ge 500 ] || fail "command palette changed only $DIFF pixels"
echo "PHASE 1 PASS: command palette painted ($DIFF px)"

type_text "split"
shot /output/02-palette-filtered.png
DIFF=$(pixel_diff /output/01-palette-open.png /output/02-palette-filtered.png)
[ "$DIFF" -ge 50 ] || fail "palette filtering changed only $DIFF pixels"
echo "PHASE 2 PASS: typing repainted the filtered rows ($DIFF px)"

send_keys Down
shot /output/03-palette-selection.png
DIFF=$(pixel_diff /output/02-palette-filtered.png /output/03-palette-selection.png)
[ "$DIFF" -ge 10 ] || fail "palette selection changed only $DIFF pixels"
echo "PHASE 3 PASS: arrow navigation repainted selection ($DIFF px)"

send_keys Escape
sleep 0.3
shot /output/03b-palette-dismissed.png

# ── Phase 4: right-click context menu ─────────────────────────────
right_click_at 300 300
shot /output/04-context-menu.png
DIFF=$(pixel_diff /output/03b-palette-dismissed.png /output/04-context-menu.png)
[ "$DIFF" -ge 200 ] || fail "context menu changed only $DIFF pixels"
echo "PHASE 4 PASS: right-click painted the context menu ($DIFF px)"

send_keys Escape
shot /output/04b-context-dismissed.png

# ── Phase 5: hover tooltip (truncated + clamped URL) ──────────────
focus
send_keys ctrl+shift+u
shot /output/05-tooltip.png
DIFF=$(pixel_diff /output/04b-context-dismissed.png /output/05-tooltip.png)
[ "$DIFF" -ge 50 ] || fail "tooltip changed only $DIFF pixels"
echo "PHASE 5 PASS: tooltip painted ($DIFF px)"

echo ""
echo "PASS: visual overlays test"
echo "  Inspect screenshots in test-output/:"
echo "    01-palette-open.png       — command palette at rest"
echo "    02-palette-filtered.png   — palette filtered by query"
echo "    03-palette-selection.png  — palette selection moved"
echo "    04-context-menu.png       — right-click context menu"
echo "    05-tooltip.png            — truncated + clamped URL tooltip"
