#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: workspace split via keybinding
#
# Presses Ctrl+Alt+\ in the real scribe-client window to trigger a
# vertical workspace split, then verifies both workspace regions are
# alive by typing into each via xdotool. It also exits a tab after moving
# focus to another region and exits the attached last tab of a region,
# pinning paint/input focus and one-shot pane adoption across both paths.
#
# Requires: visual container (optional GPU passthrough via SCRIBE_E2E_GPUS)
set -e

TITLEBAR_H=34
BOTTOM_BANDS_H=24
ROUTING_DIFF_MIN="${ROUTING_DIFF_MIN:-300}"
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

# Helper: focus the Scribe window and cache its geometry for pixel crops.
focus_window() {
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
        eval "$(xdotool getwindowgeometry --shell "$wid")"
        WIN_X="$X"
        WIN_Y="$Y"
        WIN_W="$WIDTH"
        WIN_H="$HEIGHT"
    fi
}

# Helper: focus the Scribe window and capture a full-screen screenshot.
capture_window() {
    local out="$1"
    focus_window
    sleep 0.3
    scrot -o "$out"
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

# Count differing pixels in one half of the terminal grid, excluding window
# chrome, pane borders, and the caller's bottom inset. The right pane body is
# byte-identical across a background exit; routing checks keep the prompt rows.
grid_half_diff() {
    local before="$1" after="$2" half="$3" bottom_inset="${4:-8}"
    local half_w=$((WIN_W / 2))
    local crop_x=$((WIN_X + 8))
    local crop_y=$((WIN_Y + TITLEBAR_H + 8))
    local crop_w=$((half_w - 16))
    local crop_h=$((WIN_H - TITLEBAR_H - BOTTOM_BANDS_H - 8 - bottom_inset))
    local value
    if [ "$half" = "right" ]; then
        crop_x=$((crop_x + half_w))
    fi
    value=$(compare -metric AE \
        \( "$before" -crop "${crop_w}x${crop_h}+${crop_x}+${crop_y}" +repage \) \
        \( "$after" -crop "${crop_w}x${crop_h}+${crop_x}+${crop_y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
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

# ── Phase 8: a background tab exit cannot steal window focus ───────
# Give the right region a static, cursor-free frame, then schedule the selected
# lower-region tab to exit and move focus right before its shell ends. The
# focused half must remain pixel-identical while the lower region refocuses its
# surviving sibling exactly once.
focus_window
click_at "$((WIN_W * 3 / 4))" "$((WIN_H / 2))"
xdotool type --delay 10 "printf '\\033[?25l'; clear; echo FOCUSED-RIGHT-STABLE"
xdotool key Return
sleep 0.8
capture_window /output/08a-focused-before-background-exit.png

click_at "$((WIN_W / 4))" "$((WIN_H * 3 / 4))"
adopts_before=$(log_count "pane adopted a session")
xdotool type --delay 10 "sleep 1; exit"
xdotool key Return
click_at "$((WIN_W * 3 / 4))" "$((WIN_H / 2))"
sleep 2
capture_window /output/08b-focused-after-background-exit.png
wait_bar_state "ws-[0-9a-f]+:1"
focused_diff=$(grid_half_diff \
    /output/08a-focused-before-background-exit.png \
    /output/08b-focused-after-background-exit.png right 64)
[ "$focused_diff" -eq 0 ] \
    || fail "background exit changed $focused_diff pixels in the focused region"
adopts_after=$(log_count "pane adopted a session")
[ $((adopts_after - adopts_before)) -eq 1 ] \
    || fail "background exit adopted $((adopts_after - adopts_before)) panes instead of one"
sleep 2
[ "$(log_count "pane adopted a session")" -eq "$adopts_after" ] \
    || fail "background exit kept re-adopting a pane after reconciliation"
moves_now=$(log_count "$MOVE_RE")
[ "$moves_now" -eq "$moves_after_splits" ] \
    || fail "a background tab exit re-filed a session across workspaces ($moves_after_splits -> $moves_now)"
echo "PHASE 8 PASS: background exit kept $focused_diff focused-pixel changes and adopted its sibling once"

# ── Phase 9: attached last-tab collapse adopts no dead session ─────
# Focus the lower region and exit its last tab. Region collapse must clear the
# dead active session, focus the first surviving region, and route the next
# command into that pane without any per-frame adoption loop.
click_at "$((WIN_W / 4))" "$((WIN_H * 3 / 4))"
adopts_before=$(log_count "pane adopted a session")
xdotool type --delay 30 "exit"
xdotool key Return
sleep 2
capture_window /output/09-after-region-collapse.png
wait_bar_state "none"
wait_log "closed a workspace region on the server"
sleep 2
adopts_after=$(log_count "pane adopted a session")
[ "$adopts_after" -eq "$adopts_before" ] \
    || fail "last-tab collapse adopted a dead session $((adopts_after - adopts_before)) times"
moves_now=$(log_count "$MOVE_RE")
[ "$moves_now" -eq "$moves_after_splits" ] \
    || fail "a region collapse re-filed a session across workspaces ($moves_after_splits -> $moves_now)"

# The collapsed region's replacement focus must paint and receive input from
# the same surviving session. Cursor blink can move one cell; the long command
# and its output must move substantially more pixels in the left half.
xdotool type --delay 20 "echo STILL-ALIVE-AFTER-COLLAPSE-ROUTED-TO-SURVIVOR"
xdotool key Return
sleep 0.8
capture_window /output/10-alive-after-collapse.png
routing_diff=$(grid_half_diff \
    /output/09-after-region-collapse.png \
    /output/10-alive-after-collapse.png left)
[ "$routing_diff" -ge "$ROUTING_DIFF_MIN" ] \
    || fail "last-tab collapse left routing dead ($routing_diff changed pixels)"
echo "PHASE 9 PASS: region collapse changed $routing_diff routed pixels with no pane-adopt spam"

echo ""
echo "PASS: visual workspace split test"
echo "  Inspect screenshots in test-output/:"
echo "    01-single-workspace.png   — single workspace before split"
echo "    02-after-vsplit.png        — after Ctrl+Alt+\\ (side-by-side)"
echo "    03-workspace-b-typed.png   — after typing in right workspace"
echo "    04-workspace-a-alive.png   — after typing in left workspace"
echo "    05-after-hsplit.png        — after Ctrl+Alt+- (left split top/bottom)"
echo "    07-second-tab-in-lower-bar.png — two tabs in the lower region's bar"
echo "    08a-focused-before-background-exit.png — focused-region match baseline"
echo "    08b-focused-after-background-exit.png — exact focused-region match"
echo "    09-after-region-collapse.png — after the stacked region collapsed"
echo "    10-alive-after-collapse.png — survivor accepted routed input"
echo "    06-three-workspaces.png    — all three workspaces with content"
