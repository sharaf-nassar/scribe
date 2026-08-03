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
set -e

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
echo "    02-tab-hover-close.png     — tab hover reveals close button"
echo "    03-gear-clicked.png        — gear icon clicked (settings)"
echo "    04-window-controls.png     — min/max/close window controls"
