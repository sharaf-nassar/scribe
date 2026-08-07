#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: workspace split via keybinding
#
# Presses Ctrl+Alt+\ in the real scribe-client window to trigger a
# vertical workspace split, then verifies both workspace regions are
# alive by typing into each via xdotool.  Screenshots are captured at
# each stage for visual inspection.
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

# Helper: focus the Scribe window.
focus_window() {
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
    fi
}

# Helper: click at pixel coordinates inside the Scribe window.
#
# The pointer is warped window-relative, but the button press is emitted
# through XTEST (no --window): GPUI reads pointer input through XInput2,
# which never delivers the synthetic XSendEvent buttons `click --window`
# produces — those clicks vanish while typing still works.
click_at() {
    local x="$1" y="$2"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool mousemove --window "$wid" "$x" "$y"
        xdotool click 1
        sleep 0.3
    fi
}

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# Helper: wait until the client log contains a line matching both extended
# regexes (the second defaults to matching anything). Tolerates the ANSI
# styling tracing may emit between a field name and its value.
wait_log() {
    local first="$1" second="${2:-.}"
    for _ in $(seq 40); do
        if grep -E "$first" "$CLIENT_LOG" 2>/dev/null | grep -qE "$second"; then
            return 0
        fi
        sleep 0.5
    done
    fail "client log never matched: $first / $second"
}

# Helper: wait until the LATEST "lower-region tab bars changed" line matches
# the wanted state. Earlier states stay in the log, so a plain grep would
# accept a stale match.
wait_bar_state() {
    local want="$1"
    local last=""
    for _ in $(seq 40); do
        last=$(grep -E "lower-region tab bars changed" "$CLIENT_LOG" 2>/dev/null | tail -1)
        if printf '%s' "$last" | grep -qE "$want"; then
            return 0
        fi
        sleep 0.5
    done
    fail "latest bar state never matched '$want' (got: $last)"
}

# Helper: count client-log lines matching an extended regex.
log_count() {
    grep -cE "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# A session is re-filed on the server once per workspace split (the seed
# session is created through the previous region's workspace and moved when
# its pane adopts it). Anything beyond that is the exit-refocus bug dragging
# a surviving session across a workspace boundary.
MOVE_RE="moved a session into another workspace region"

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
# Click into the left workspace first, then split. The stacked lower region
# must reserve its own in-region tab bar; the client logs the bar set
# whenever it changes, which is the scripted oracle for chrome the
# screenshots only show to a human.
click_at 480 540
sleep 0.3
xdotool key --clearmodifiers ctrl+alt+minus
sleep 1.5
capture_window /output/05-after-hsplit.png
wait_bar_state "ws-[0-9a-f]+:1"
echo "PHASE 5 PASS: horizontal split created a lower region with a 1-tab bar"

# ── Phase 6: type into the bottom-left workspace (newest) ───────────
focus_window
sleep 0.3
xdotool type --delay 30 "echo WORKSPACE-C"
xdotool key Return
sleep 0.8
capture_window /output/06-three-workspaces.png
echo "PHASE 6 PASS: typed into third workspace, screenshot captured"

# Both splits have landed, so exactly two sessions were re-filed (each
# split's seed moving into its freshly minted workspace). Record the count
# before the exit phases: it must not grow again.
moves_after_splits=$(log_count "$MOVE_RE")
[ "$moves_after_splits" -eq 2 ] \
    || fail "expected 2 split-seed session moves, saw $moves_after_splits"

# ── Phase 7: second tab in the stacked workspace grows its bar ──────
focus_window
sleep 0.3
xdotool key --clearmodifiers ctrl+shift+t
sleep 1.5
capture_window /output/07-second-tab-in-lower-bar.png
wait_bar_state "ws-[0-9a-f]+:2"
echo "PHASE 7 PASS: new tab joined the lower region's bar"

# ── Phase 7b: clicking the bar's inactive tab selects its session ───
# The lower-left region's bar sits at the top of the bottom-left region:
# titlebar (34px) + half the grid band puts it around y=400 in this
# 1008x756 window, and the first (inactive) tab starts at the bar's left
# edge. The click must reach the bar's own handler, which logs the
# session it selects.
click_at 90 400
sleep 1
wait_log "region bar selected a tab"
capture_window /output/07b-bar-tab-clicked.png
echo "PHASE 7b PASS: clicking the lower bar's tab reached its session"

# ── Phase 8: exiting one tab keeps the refocus inside the workspace ─
focus_window
sleep 0.3
xdotool type --delay 30 "exit"
xdotool key Return
sleep 2
capture_window /output/08-after-tab-exit.png
wait_bar_state "ws-[0-9a-f]+:1"
moves_now=$(log_count "$MOVE_RE")
[ "$moves_now" -eq "$moves_after_splits" ] \
    || fail "a tab exit re-filed a session across workspaces ($moves_after_splits -> $moves_now)"
echo "PHASE 8 PASS: tab exit refocused within its workspace, no session moved"

# ── Phase 9: collapsing the stacked region moves no session either ──
focus_window
sleep 0.3
xdotool type --delay 30 "exit"
xdotool key Return
sleep 2
capture_window /output/09-after-region-collapse.png
wait_bar_state "none"
wait_log "closed a workspace region on the server"
moves_now=$(log_count "$MOVE_RE")
[ "$moves_now" -eq "$moves_after_splits" ] \
    || fail "a region collapse re-filed a session across workspaces ($moves_after_splits -> $moves_now)"
# The surviving regions must still accept input after the collapse.
focus_window
sleep 0.3
xdotool type --delay 30 "echo STILL-ALIVE-AFTER-COLLAPSE"
xdotool key Return
sleep 0.8
capture_window /output/10-alive-after-collapse.png
echo "PHASE 9 PASS: region collapsed cleanly, no session changed workspace"

echo ""
echo "PASS: visual workspace split test"
echo "  Inspect screenshots in test-output/:"
echo "    01-single-workspace.png   — single workspace before split"
echo "    02-after-vsplit.png        — after Ctrl+Alt+\\ (side-by-side)"
echo "    03-workspace-b-typed.png   — after typing in right workspace"
echo "    04-workspace-a-alive.png   — after typing in left workspace"
echo "    05-after-hsplit.png        — after Ctrl+Alt+- (left split top/bottom)"
echo "    07-second-tab-in-lower-bar.png — two tabs in the lower region's bar"
echo "    08-after-tab-exit.png      — after exiting one lower-region tab"
echo "    09-after-region-collapse.png — after the stacked region collapsed"
echo "    06-three-workspaces.png    — all three workspaces with content"
