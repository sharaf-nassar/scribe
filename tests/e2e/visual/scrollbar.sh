#!/bin/bash
# Scripted E2E: the command-mark overlay scrollbar is painted by the real client.
#
# `scrollbar.rs` landed with a green unit suite and zero references outside its
# own module — the 016 reachability audit listed it as an unwired module, and
# the running client painted no scrollbar at all. A `#[gpui::test]` over the
# thumb geometry proves the math and says nothing about whether the binary can
# reach it, so every phase below drives the real window and asserts pixels in
# the pane's right-edge strip: the one region only the wired paint path writes.
#
# The window is driven through XTEST (`xdotool key` / `xdotool mousemove`, no
# `--window`): GPUI reads input through XInput2 and ignores the synthetic
# XSendEvent input `xdotool --window` delivers, so window-targeted input would
# leave the client untouched while the script still "passed".
#
# Hover is what makes the assertions deterministic rather than a race against
# the fade: `on_hover_enter` pins the overlay fully opaque and clears the idle
# timer, so the strip can be captured, re-captured and compared without the
# 1.5 s idle delay expiring underneath the screenshots.
#
# Requires the shared-pane rig:
#   just e2e-visual-scrollbar
# which exports SESSION and joins the client to the daemon's window, so the OSC
# 133 bytes a real shell writes land in the very pane being measured.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Lit pixels the grid must hold before any phase runs, measured the same way
# terminal-viewport.sh measures it.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Width of the right-edge strip every pixel assertion is made in. The thumb is
# 6 px wide (9 px hovered) inset 2 px from the pane's right edge, so 24 px is
# comfortably wider than the overlay and still narrow enough that ordinary
# terminal text never reaches it (the filler rows below stay short).
STRIP_W="${STRIP_W:-24}"

# Bands excluded from the strip: the integrated tab strip above the grid and
# the status line plus system-stats bar below it, none of which the scrollbar
# is painted into and all of which repaint on timers of their own.
GRID_TOP_INSET_PX="${GRID_TOP_INSET_PX:-40}"
GRID_BOTTOM_INSET_PX="${GRID_BOTTOM_INSET_PX:-80}"

# Pixels the thumb must change in the strip. The resting thumb is 6x(>=20) px at
# 40 % alpha over a near-black background; an unpainted scrollbar changes zero.
THUMB_DIFF_MIN="${THUMB_DIFF_MIN:-60}"

# Pixels one command tick must contribute. A tick is 2 px tall across the thumb
# width, so a single mark is ~12-18 px; the floor is deliberately below one
# tick so the assertion fails on "no ticks" rather than on antialiasing.
TICK_PIXELS_MIN="${TICK_PIXELS_MIN:-4}"

# How far a channel must lead the other two for a pixel to count as a coloured
# tick. Ticks paint at 24 % alpha (40 % thumb alpha x the 0.6 tick scale) over
# the theme background, which puts minimal-dark's ANSI red (#ef4444) and green
# (#22c55e) roughly 35-40 counts ahead of their siblings.
TICK_CHANNEL_LEAD="${TICK_CHANNEL_LEAD:-18}"

POLL_TICKS="${POLL_TICKS:-20}"

# Rows of filler between consecutive prompt marks, and the row count the AI
# trim epoch has to exceed before the server trims anything back.
FILL_ROWS="${FILL_ROWS:-30}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found" >&2
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Capture the client window only. A full-screen scrot also catches openbox's
# title bar, whose pixels belong to no phase here.
#
# Deliberately does NOT re-focus: `windowactivate` warps nothing, but the
# scrollbar phases hold the pointer inside the hit zone and a re-focus round
# trip is dead time the fade would otherwise be running through. `focus` is
# called explicitly by the phases that need fresh geometry.
capture() {
    sleep 0.4
    scrot -o /output/scrollbar-fullscreen.png
    convert /output/scrollbar-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# The pane's right-edge strip, cropped out of a full-window capture: the only
# region the overlay scrollbar writes into.
strip() {
    local h
    h=$(( WIN_H - GRID_TOP_INSET_PX - GRID_BOTTOM_INSET_PX ))
    convert "$1" \
        -crop "${STRIP_W}x${h}+$(( WIN_W - STRIP_W ))+${GRID_TOP_INSET_PX}" +repage "$2"
}

strip_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

# Emit the strip-relative Y of every pixel whose $1 channel leads the other two
# by at least TICK_CHANNEL_LEAD counts — i.e. every pixel carrying a success
# (green) or failure (red) tick, as opposed to the neutral grey thumb or the
# near-black cells behind it.
#
# `convert … txt:` plus sed rather than a gawk regex capture: the test image
# ships mawk, which has no three-argument `match`.
tick_hits() {
    local channel="$1" file="$2"
    convert "$file" -depth 8 -alpha off txt:- \
        | sed -n 's/^[0-9]*,\([0-9]*\): (\([0-9]*\),\([0-9]*\),\([0-9]*\)).*/\1 \2 \3 \4/p' \
        | awk -v ch="$channel" -v lead="$TICK_CHANNEL_LEAD" '
            {
                y = $1; r = $2; g = $3; b = $4
                if (ch == "red") { own = r; a = g; c = b } else { own = g; a = r; c = b }
                if (own - a >= lead && own - c >= lead) { print y }
            }'
}

# How many pixels of a $1-coloured tick the strip holds.
tick_pixels() {
    tick_hits "$1" "$2" | wc -l | tr -d ' '
}

# The distinct strip-relative rows those pixels sit on. Comparing the topmost
# tick row before and after a trim is how "the marks shifted" is observed as
# pixels rather than as a log line.
tick_rows() {
    tick_hits "$1" "$2" | sort -n | uniq
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

# Park the pointer inside the scrollbar's hit zone (the 3x-width band anchored
# to the pane's right edge), which pins the overlay open and widens the thumb.
hover_scrollbar() {
    xdotool mousemove $(( WIN_X + WIN_W - 6 )) $(( WIN_Y + WIN_H / 2 ))
    sleep 0.5
}

# Park the pointer far from the right edge so the overlay is not being held
# open by hover.
unhover_scrollbar() {
    xdotool mousemove $(( WIN_X + WIN_W / 4 )) $(( WIN_Y + WIN_H / 2 ))
    sleep 0.5
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# The most recent client log line containing $1, with the tracing formatter's
# ANSI colour codes stripped.
last_log_line() {
    grep -F "$1" "$CLIENT_LOG" 2>/dev/null | tail -1 | sed -e 's/\x1b\[[0-9;]*m//g'
}

log_field() {
    printf '%s' "$1" | sed -n "s/.*[ \t]$2=\([0-9-][0-9]*\).*/\1/p"
}

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -20 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

# `scribe-test send` turns `\n` into a real Enter and leaves every other
# backslash escape untouched, so the shell's own printf produces the OSC bytes.
osc133_a() {
    scribe-test send "$SESSION" "printf '\033]133;A\007'\n"
    sleep 0.5
}

osc133_d() {
    scribe-test send "$SESSION" "printf '\033]133;D;$1\007'\n"
    sleep 0.5
}

# One command's worth of scrollback, gated on its own echoed sentinel so the
# next mark is never emitted into a still-filling grid. The rows are kept short
# so no glyph ever reaches the right-edge strip the assertions read.
fill_rows() {
    local tag="$1"
    scribe-test send "$SESSION" \
        "i=1; while [ \$i -le $FILL_ROWS ]; do echo \"sb-$tag-\$i\"; i=\$((i+1)); done; echo SB_FILLED_$tag\n"
    scribe-test wait-output "$SESSION" "SB_FILLED_$tag" --timeout 20000
    sleep 0.5
}

# One complete command record: prompt start, output, command end reporting $2
# as the exit code.
command_record() {
    osc133_a
    fill_rows "$1"
    osc133_d "$2"
}

# ── Phase 0: the shared pane is painted ───────────────────────────
focus
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/sb-00-attached.png
    ink=$(window_ink /output/sb-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: a rested pane shows no overlay, and holds still ──────
# The control every later strip diff is measured against. With the pointer
# parked away from the right edge and the idle window elapsed, the fade has
# taken the overlay to zero opacity and `build_scrollbar_render` emits nothing
# — so the strip is whatever the cells underneath it paint. Capturing it twice
# proves the strip is genuinely quiet: without that, a thumb assertion could be
# satisfied by any repaint that happens to touch the right edge.
unhover_scrollbar
sleep 2.5
capture /output/sb-01-rested.png
strip /output/sb-01-rested.png /output/sb-01-strip.png
capture /output/sb-01-rested-again.png
strip /output/sb-01-rested-again.png /output/sb-01-strip-again.png
DIFF=$(strip_diff /output/sb-01-strip.png /output/sb-01-strip-again.png)
if [ "${DIFF:-0}" -ge "$THUMB_DIFF_MIN" ]; then
    fail "PHASE 1 FAIL: the rested scrollbar strip is not stable (${DIFF}px between two captures)"
fi
echo "PHASE 1 PASS: a rested pane paints no overlay and the strip is stable (${DIFF}px)"

# ── Phase 2: three OSC 133 command records fill the scrollback ────
# The middle command exits non-zero, so the three ticks must not all be the
# same hue. `prompt mark recorded` is written only by the drain's PromptMark
# arm, so the assertion is on ingested state rather than on wire traffic.
BASE=$(count_log "prompt mark recorded")
command_record c1 0
command_record c2 3
command_record c3 0
if ! wait_for_log_growth "prompt mark recorded" "$BASE" 30; then
    fail "PHASE 2 FAIL: no PromptMark reached the client"
fi
LINE=$(last_log_line "prompt mark recorded")
MARKS=$(log_field "$LINE" marks)
if [ "${MARKS:-0}" -lt 3 ]; then
    fail "PHASE 2 FAIL: expected 3 command records, the client holds ${MARKS:-0}: $LINE"
fi
echo "PHASE 2 PASS: the client ingested $MARKS command records"

# ── Phase 3: the thumb paints once the pane has scrollback ────────
# Scrolling up both gives the thumb somewhere to sit and pulses the overlay
# open; the hover then holds it there for the capture.
focus
send_keys shift+Prior
hover_scrollbar
capture /output/sb-03-thumb.png
strip /output/sb-03-thumb.png /output/sb-03-strip.png
DIFF=$(strip_diff /output/sb-01-strip.png /output/sb-03-strip.png)
if [ "${DIFF:-0}" -lt "$THUMB_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: the scrollbar strip changed only ${DIFF}px (min $THUMB_DIFF_MIN); no thumb was painted"
fi
echo "PHASE 3 PASS: the overlay scrollbar painted ${DIFF}px into the right-edge strip"

# ── Phase 4: the command ticks carry their status colours ─────────
# Success takes the theme's ANSI green and failure its ANSI red, so a strip
# holding both proves the marks reached the paint path AND that the A -> D exit
# codes were resolved: a neutral-only strip would mean the ticks render but
# every command stayed `Unknown`.
GREEN=$(tick_pixels green /output/sb-03-strip.png)
RED=$(tick_pixels red /output/sb-03-strip.png)
if [ "${GREEN:-0}" -lt "$TICK_PIXELS_MIN" ]; then
    fail "PHASE 4 FAIL: no success tick in the strip (${GREEN}px green, min $TICK_PIXELS_MIN)"
fi
if [ "${RED:-0}" -lt "$TICK_PIXELS_MIN" ]; then
    fail "PHASE 4 FAIL: no failure tick in the strip (${RED}px red, min $TICK_PIXELS_MIN)"
fi
FIRST_GREEN=$(tick_rows green /output/sb-03-strip.png | head -1)
FIRST_RED=$(tick_rows red /output/sb-03-strip.png | head -1)
echo "PHASE 4 PASS: success (${GREEN}px, first row $FIRST_GREEN) and failure (${RED}px, first row $FIRST_RED) ticks are on screen"

# ── Phase 5: a scrollback trim shifts every surviving tick ────────
# The server only trims an AI session's scrollback: the first suppressed ED 3
# records a baseline history and a later one trims back to it. So the pane is
# armed as an AI session, an ED 3 sets the baseline, plain output grows the
# history past it, and a second ED 3 makes the server drop the difference. The
# client replicates that drop on its own grid and shifts every surviving mark
# by it, which is what moves every tick up the track.
#
# The filler between the two ED 3s is deliberately plain `echo` with no OSC 133:
# a `PromptStart` means the AI tool exited and the shell prompt is back, so the
# server clears the provider and the trim baseline with it, and the second ED 3
# would not be filtered at all.
BEFORE_GREEN_ROW=$(tick_rows green /output/sb-03-strip.png | head -1)
scribe-test send "$SESSION" "printf '\033]1337;ScribeAiLaunch=claude_code\007\033[3J'; echo SB_EPOCH\n"
scribe-test wait-output "$SESSION" "SB_EPOCH" --timeout 20000
sleep 1
fill_rows c4
BASE=$(count_log "trimmed scrollback marks")
scribe-test send "$SESSION" "printf '\033[3J'; echo SB_TRIMMED\n"
scribe-test wait-output "$SESSION" "SB_TRIMMED" --timeout 20000
if ! wait_for_log_growth "trimmed scrollback marks" "$BASE" 25; then
    fail "PHASE 5 FAIL: the server's TrimScrollback never dropped a row on the client"
fi
LINE=$(last_log_line "trimmed scrollback marks")
DROPPED=$(log_field "$LINE" dropped)
REMAINING=$(log_field "$LINE" marks)
if [ "${DROPPED:-0}" -le 0 ]; then
    fail "PHASE 5 FAIL: the trim reported no dropped rows: $LINE"
fi
if [ "${REMAINING:-0}" -lt 1 ]; then
    fail "PHASE 5 FAIL: the trim retired every mark, so there is nothing left to tick: $LINE"
fi
focus
send_keys shift+Prior
hover_scrollbar
capture /output/sb-05-after-trim.png
strip /output/sb-05-after-trim.png /output/sb-05-strip.png
AFTER_GREEN=$(tick_pixels green /output/sb-05-strip.png)
if [ "${AFTER_GREEN:-0}" -lt "$TICK_PIXELS_MIN" ]; then
    fail "PHASE 5 FAIL: $REMAINING marks survived the trim but no tick is on screen"
fi
AFTER_GREEN_ROW=$(tick_rows green /output/sb-05-strip.png | head -1)
if [ "${AFTER_GREEN_ROW:-0}" = "${BEFORE_GREEN_ROW:-0}" ]; then
    fail "PHASE 5 FAIL: the topmost success tick stayed on row $BEFORE_GREEN_ROW; the marks never shifted"
fi
echo "PHASE 5 PASS: the trim dropped $DROPPED rows, $REMAINING marks survived, and the topmost success tick moved from row $BEFORE_GREEN_ROW to $AFTER_GREEN_ROW"

# ── Phase 6: the overlay fades out when the pointer leaves ────────
# The fade is the reason the scrollbar can be an overlay at all: it must go
# away on its own, or it would permanently cover the rightmost cells. Leaving
# the hit zone re-arms the idle timer, and past the 1.5 s delay plus the 0.3 s
# ramp the strip has to be back to what an unscrolled pane painted.
unhover_scrollbar
sleep 2.5
capture /output/sb-06-faded.png
strip /output/sb-06-faded.png /output/sb-06-strip.png
FADED=$(strip_diff /output/sb-01-strip.png /output/sb-06-strip.png)
if [ "${FADED:-0}" -ge "$THUMB_DIFF_MIN" ]; then
    fail "PHASE 6 FAIL: the scrollbar never faded out (${FADED}px still painted in the strip)"
fi
echo "PHASE 6 PASS: the overlay faded back out after the pointer left (${FADED}px in the strip)"

echo ""
echo "PASS: visual scrollbar test"
echo "  Inspect screenshots in test-output/:"
echo "    sb-00-attached.png     — the shared pane on attach"
echo "    sb-01-rested.png       — the rested strip, with the overlay faded out"
echo "    sb-03-thumb.png        — the thumb and command ticks on screen"
echo "    sb-05-after-trim.png   — the ticks after a server scrollback trim"
echo "    sb-06-faded.png        — the overlay faded back out"
