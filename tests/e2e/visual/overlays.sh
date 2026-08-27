#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: command palette, context menu, and failed-link annotation
# demo states. Ctrl+Shift+U cycles the real annotation paint state through the
# mock's default, busy-row, clamped, and top-flip placement grammar.
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

# ── Phase 5: failed-link annotation demo cycle ─────────────────────
# Leave a real row for the busy-row state to cover. The demo itself only builds
# the same anchor/lifecycle state as a failed opener; it never writes terminal
# content.
focus
type_text "echo annotation-demo-busy-row"
send_keys Return
sleep 0.5
shot /output/04c-annotation-demo-fixture.png

send_keys ctrl+shift+u
shot /output/05-annotation-demo-default.png
DIFF=$(pixel_diff /output/04c-annotation-demo-fixture.png /output/05-annotation-demo-default.png)
[ "$DIFF" -ge 50 ] || fail "annotation-demo-default changed only $DIFF pixels"
echo "PHASE 5 PASS: annotation-demo-default painted ($DIFF px)"

send_keys ctrl+shift+u
shot /output/06-annotation-demo-busy-row.png
DIFF=$(pixel_diff /output/05-annotation-demo-default.png /output/06-annotation-demo-busy-row.png)
[ "$DIFF" -ge 50 ] || fail "annotation-demo-busy-row changed only $DIFF pixels"
echo "PHASE 6 PASS: annotation-demo-busy-row painted ($DIFF px)"

send_keys ctrl+shift+u
shot /output/07-annotation-demo-clamped.png
DIFF=$(pixel_diff /output/06-annotation-demo-busy-row.png /output/07-annotation-demo-clamped.png)
[ "$DIFF" -ge 50 ] || fail "annotation-demo-clamped changed only $DIFF pixels"
echo "PHASE 7 PASS: annotation-demo-clamped painted ($DIFF px)"

send_keys ctrl+shift+u
shot /output/08-annotation-demo-top-flip.png
DIFF=$(pixel_diff /output/07-annotation-demo-clamped.png /output/08-annotation-demo-top-flip.png)
[ "$DIFF" -ge 50 ] || fail "annotation-demo-top-flip changed only $DIFF pixels"
echo "PHASE 8 PASS: annotation-demo-top-flip painted ($DIFF px)"

echo ""
echo "PASS: visual overlays test"
echo "  Inspect screenshots in test-output/:"
echo "    01-palette-open.png                   — command palette at rest"
echo "    02-palette-filtered.png               — palette filtered by query"
echo "    03-palette-selection.png              — palette selection moved"
echo "    04-context-menu.png                   — right-click context menu"
echo "    05-annotation-demo-default.png        — head anchor above the run"
echo "    06-annotation-demo-busy-row.png       — opaque band over terminal text"
echo "    07-annotation-demo-clamped.png        — tail anchor with ─┐"
echo "    08-annotation-demo-top-flip.png       — top-edge flip with └"
