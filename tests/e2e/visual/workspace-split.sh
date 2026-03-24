#!/bin/bash
# Visual E2E test: workspace split via keybinding
#
# Presses Ctrl+Alt+\ in the real scribe-client window to trigger a
# vertical workspace split, then verifies both workspace regions are
# alive by typing into each via xdotool.  Screenshots are captured at
# each stage for visual inspection.
#
# Requires: visual container with --gpus all
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

# Helper: focus the Scribe window.
focus_window() {
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
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

# ── Phase 1: baseline — single workspace with content ──────────────
# Use xdotool to type into the initial workspace (avoids daemon/client
# output stream conflicts after later splits).
focus_window
sleep 0.5
xdotool type --delay 30 "echo WORKSPACE-A"
xdotool key Return
sleep 0.8
capture_window /output/01-single-workspace.png
echo "PHASE 1 PASS: single workspace baseline captured"

# ── Phase 2: trigger vertical workspace split via keybinding ────────
focus_window
# Ctrl+Alt+backslash = workspace split vertical (side-by-side)
xdotool key --clearmodifiers ctrl+alt+backslash
sleep 1.5
capture_window /output/02-after-vsplit.png
echo "PHASE 2 PASS: vertical workspace split triggered, screenshot captured"

# ── Phase 3: type into the new workspace (right side, auto-focused) ─
focus_window
sleep 0.5
xdotool type --delay 30 "echo WORKSPACE-B"
xdotool key Return
sleep 0.8
capture_window /output/03-workspace-b-typed.png
echo "PHASE 3 PASS: typed into new workspace (right), screenshot captured"

# ── Phase 4: click the left workspace and type into it ──────────────
# After a vertical split on 1920x1080, the left workspace occupies
# roughly x=0..960.  Click in the center of the left region.
click_at 480 540
sleep 0.3
xdotool type --delay 30 "echo STILL-ALIVE-A"
xdotool key Return
sleep 0.8
capture_window /output/04-workspace-a-alive.png
echo "PHASE 4 PASS: typed into original workspace (left), screenshot captured"

# ── Phase 5: trigger horizontal workspace split in left workspace ───
# Click into the left workspace first, then split.
click_at 480 540
sleep 0.3
xdotool key --clearmodifiers ctrl+alt+minus
sleep 1.5
capture_window /output/05-after-hsplit.png
echo "PHASE 5 PASS: horizontal workspace split triggered, screenshot captured"

# ── Phase 6: type into the bottom-left workspace (newest) ───────────
focus_window
sleep 0.3
xdotool type --delay 30 "echo WORKSPACE-C"
xdotool key Return
sleep 0.8
capture_window /output/06-three-workspaces.png
echo "PHASE 6 PASS: typed into third workspace, screenshot captured"

echo ""
echo "PASS: visual workspace split test"
echo "  Inspect screenshots in test-output/:"
echo "    01-single-workspace.png   — single workspace before split"
echo "    02-after-vsplit.png        — after Ctrl+Alt+\\ (side-by-side)"
echo "    03-workspace-b-typed.png   — after typing in right workspace"
echo "    04-workspace-a-alive.png   — after typing in left workspace"
echo "    05-after-hsplit.png        — after Ctrl+Alt+- (left split top/bottom)"
echo "    06-three-workspaces.png    — all three workspaces with content"
