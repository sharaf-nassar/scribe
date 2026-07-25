#!/bin/bash
# Scripted E2E: OSC 133 prompt marks are ingested and the three mark-relative
# jumps move the real client's viewport.
#
# The 016 reachability audit found `ServerMessage::PromptMark` and
# `ServerMessage::ScrollBottom` falling into the reader's drop counter and
# `PromptJumpUp` / `PromptJumpDown` / `JumpToFailure` falling into the key
# path's swallow arm, so the client had neither marks nor a way to reach them.
# Every phase below therefore drives the real window and asserts something only
# the wired path can produce: a recorded mark, a viewport that lands on the row
# a specific mark named, and a server-driven snap back to the live bottom.
#
# The window is driven through XTEST (`xdotool key`, no `--window`): GPUI reads
# the keyboard through XInput2 and ignores the synthetic XSendEvent input that
# `xdotool --window` delivers, so window-targeted input would leave the client
# untouched while the script still "passed".
#
# Marks come from a real shell writing real OSC 133 into the pane — the exact
# byte path a shell-integration prompt uses — so the server's OSC interceptor,
# its `PromptMark` emission, and the client's ingestion are all on trial.
#
# Requires the shared-pane rig:
#   just e2e-visual-prompt-marks
# which exports SESSION and joins the client to the daemon's window.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Lit pixels the grid must hold before any phase runs, measured the same way
# terminal-viewport.sh measures it.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Differing pixels a whole-viewport repaint must produce. A jump replaces every
# text row; a swallowed chord leaves consecutive frames byte-identical (the
# image pins SCRIBE_DISABLE_ANIMATIONS=1).
VIEWPORT_DIFF_MIN="${VIEWPORT_DIFF_MIN:-5000}"

POLL_TICKS="${POLL_TICKS:-20}"

# Rows of filler between consecutive prompt marks. Large enough that landing on
# one mark rather than its neighbour is a whole-screen repaint.
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
capture() {
    focus
    sleep 0.4
    scrot -o /output/marks-fullscreen.png
    convert /output/marks-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

window_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# Wait until the client log holds more copies of a pattern than `baseline`.
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
# ANSI colour codes stripped — they sit between the field name and its value
# (`offset\e[0m=\e[0m1`), so an unstripped line matches no `field=value` test.
last_log_line() {
    grep -F "$1" "$CLIENT_LOG" 2>/dev/null | tail -1 | sed -e 's/\x1b\[[0-9;]*m//g'
}

# Read one numeric `name=value` field out of a stripped log line.
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
# backslash escape (`\033`, `\007`) untouched, so the shell receives the escape
# spelling verbatim and its own printf produces the OSC bytes.
osc133_a() {
    scribe-test send "$SESSION" "printf '\033]133;A\007'\n"
    sleep 0.5
}

osc133_d() {
    scribe-test send "$SESSION" "printf '\033]133;D;$1\007'\n"
    sleep 0.5
}

# The absolute scrollback row of the Nth ingested `PromptStart` mark, read back
# out of the client's own ingestion log.
#
# Deriving the expected landing row from the client rather than hard-coding it
# keeps the jump assertions independent of the window's row count, which decides
# how many marks sit above the live viewport.
prompt_start_abs() {
    local line history cursor
    line=$(grep -F "prompt mark recorded" "$CLIENT_LOG" 2>/dev/null \
        | sed -e 's/\x1b\[[0-9;]*m//g' | grep -F "kind=PromptStart" | sed -n "$1p")
    history=$(log_field "$line" history)
    cursor=$(log_field "$line" cursor_row)
    printf '%s' "$(( ${history:-0} + ${cursor:-0} ))"
}

# One command's worth of scrollback between two marks, gated on its own echoed
# sentinel so the next mark is never emitted into a still-filling grid.
#
# Each row carries a bar that grows by `$2` characters per row, so a block's
# rows differ from each other AND every block differs in shape from its
# neighbours. Uniform filler would make two viewports parked on different marks
# near-identical images, and the repaint assertions meaningless.
fill_rows() {
    local tag="$1" step="$2"
    scribe-test send "$SESSION" \
        "i=1; bar=; while [ \$i -le $FILL_ROWS ]; do j=1; while [ \$j -le $step ]; do bar=\"\$bar#\"; j=\$((j+1)); done; echo \"pm-$tag-row-\$i \$bar\"; i=\$((i+1)); done; echo PM_FILLED_$tag\n"
    scribe-test wait-output "$SESSION" "PM_FILLED_$tag" --timeout 20000
    sleep 0.5
}

# One complete command record: prompt start, output whose bars grow by $3 per
# row, command end reporting $2 as the exit code.
command_record() {
    osc133_a
    fill_rows "$1" "$3"
    osc133_d "$2"
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/pm-00-attached.png
    ink=$(window_ink /output/pm-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: jump_to_failure with no marks is a no-op (FR-011) ────
# The chord has to REACH the handler and then decline to move, which is a
# different observation from the chord being swallowed: only the wired handler
# writes "prompt jump found no mark".
focus
BASE=$(count_log "prompt jump found no mark")
send_keys ctrl+shift+b
if ! wait_for_log_growth "prompt jump found no mark" "$BASE"; then
    fail "PHASE 1 FAIL: ctrl+shift+b never reached jump_to_failure (still swallowed?)"
fi
LINE=$(last_log_line "prompt jump found no mark")
case "$LINE" in
    *"action=\"jump_to_failure\""*) ;;
    *) fail "PHASE 1 FAIL: the no-mark signal came from the wrong action: $LINE" ;;
esac
capture /output/pm-01-no-marks.png
DIFF=$(window_diff /output/pm-00-attached.png /output/pm-01-no-marks.png)
echo "PHASE 1 PASS: jump_to_failure with no marks left the viewport alone (${DIFF}px) — $LINE"

# ── Phase 2: the client ingests real OSC 133 marks ────────────────
# Three commands, middle one failing. `prompt mark recorded` is written only by
# the drain's PromptMark arm, and its `marks=` field is the live record count,
# so the assertion is on ingested state rather than on wire traffic.
BASE=$(count_log "prompt mark recorded")
command_record c1 0 1
command_record c2 3 2
command_record c3 0 3
if ! wait_for_log_growth "prompt mark recorded" "$BASE" 30; then
    fail "PHASE 2 FAIL: no PromptMark reached the client (still in the drop counter?)"
fi
LINE=$(last_log_line "prompt mark recorded")
MARKS=$(log_field "$LINE" marks)
if [ "${MARKS:-0}" -lt 3 ]; then
    fail "PHASE 2 FAIL: expected 3 command records, the client holds ${MARKS:-0}: $LINE"
fi
C1_ROW=$(prompt_start_abs 1)
C2_ROW=$(prompt_start_abs 2)
C3_ROW=$(prompt_start_abs 3)
if [ "$C1_ROW" -ge "$C2_ROW" ] || [ "$C2_ROW" -ge "$C3_ROW" ]; then
    fail "PHASE 2 FAIL: the three marks are not strictly ordered ($C1_ROW, $C2_ROW, $C3_ROW)"
fi
capture /output/pm-02-marks-emitted.png
echo "PHASE 2 PASS: the client ingested $MARKS command records at rows $C1_ROW/$C2_ROW/$C3_ROW"

# ── Phase 3: prompt_jump_up walks the marks upward ────────────────
send_keys shift+End
capture /output/pm-03-bottom.png
BASE=$(count_log "prompt jump moved")
send_keys ctrl+shift+z
if ! wait_for_log_growth "prompt jump moved" "$BASE"; then
    fail "PHASE 3 FAIL: ctrl+shift+z never reached prompt_jump_up (still swallowed?)"
fi
LINE=$(last_log_line "prompt jump moved")
case "$LINE" in
    *"action=\"prompt_jump_up\""*) ;;
    *) fail "PHASE 3 FAIL: ctrl+shift+z resolved to the wrong action: $LINE" ;;
esac
case "$LINE" in
    *"moved=true"*) ;;
    *) fail "PHASE 3 FAIL: the jump reported no viewport movement: $LINE" ;;
esac
UP1_TARGET=$(log_field "$LINE" target)
UP1_OFFSET=$(log_field "$LINE" offset)
if [ "${UP1_OFFSET:-0}" -le 0 ]; then
    fail "PHASE 3 FAIL: the viewport stayed at the live bottom: $LINE"
fi
capture /output/pm-03-jump-up-1.png
DIFF=$(window_diff /output/pm-03-bottom.png /output/pm-03-jump-up-1.png)
if [ "${DIFF:-0}" -lt "$VIEWPORT_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: the jump changed $DIFF px (min $VIEWPORT_DIFF_MIN); the grid never repainted"
fi
# A second press must land on the NEXT mark up, not re-select the same one.
BASE=$(count_log "prompt jump moved")
send_keys ctrl+shift+z
if ! wait_for_log_growth "prompt jump moved" "$BASE"; then
    fail "PHASE 3 FAIL: the second ctrl+shift+z produced no jump"
fi
LINE=$(last_log_line "prompt jump moved")
UP2_TARGET=$(log_field "$LINE" target)
if [ "${UP2_TARGET:-0}" -ge "${UP1_TARGET:-0}" ]; then
    fail "PHASE 3 FAIL: the second jump did not move further up ($UP1_TARGET → $UP2_TARGET)"
fi
capture /output/pm-03-jump-up-2.png
echo "PHASE 3 PASS: ctrl+shift+z walked the marks up (row $UP1_TARGET → $UP2_TARGET, +$DIFF px)"

# ── Phase 4: prompt_jump_down walks back toward the live bottom ───
BASE=$(count_log "prompt jump moved")
send_keys ctrl+shift+x
if ! wait_for_log_growth "prompt jump moved" "$BASE"; then
    fail "PHASE 4 FAIL: ctrl+shift+x never reached prompt_jump_down (still swallowed?)"
fi
LINE=$(last_log_line "prompt jump moved")
case "$LINE" in
    *"action=\"prompt_jump_down\""*) ;;
    *) fail "PHASE 4 FAIL: ctrl+shift+x resolved to the wrong action: $LINE" ;;
esac
DOWN_TARGET=$(log_field "$LINE" target)
if [ "${DOWN_TARGET:-0}" != "${UP1_TARGET:-0}" ]; then
    fail "PHASE 4 FAIL: jumping down landed on row $DOWN_TARGET, not back on $UP1_TARGET: $LINE"
fi
capture /output/pm-04-jump-down.png
DIFF=$(window_diff /output/pm-03-jump-up-2.png /output/pm-04-jump-down.png)
if [ "${DIFF:-0}" -lt "$VIEWPORT_DIFF_MIN" ]; then
    fail "PHASE 4 FAIL: jumping down changed $DIFF px (min $VIEWPORT_DIFF_MIN)"
fi
echo "PHASE 4 PASS: ctrl+shift+x returned to row $DOWN_TARGET (+$DIFF px)"

# ── Phase 5: jump_to_failure picks the failed command ─────────────
# Only the MIDDLE command exited non-zero, so the landing row separates the
# wired behaviour from both cheap imitations: the newest mark (what a plain
# jump-up reaches) and the oldest (what "take the first record" would give).
send_keys shift+End
BASE=$(count_log "prompt jump moved")
send_keys ctrl+shift+b
if ! wait_for_log_growth "prompt jump moved" "$BASE"; then
    fail "PHASE 5 FAIL: ctrl+shift+b produced no jump even though a command failed"
fi
LINE=$(last_log_line "prompt jump moved")
case "$LINE" in
    *"action=\"jump_to_failure\""*) ;;
    *) fail "PHASE 5 FAIL: ctrl+shift+b resolved to the wrong action: $LINE" ;;
esac
FAIL_TARGET=$(log_field "$LINE" target)
if [ "${FAIL_TARGET:-0}" != "$C2_ROW" ]; then
    fail "PHASE 5 FAIL: expected the failed command at row $C2_ROW, landed on $FAIL_TARGET (marks: $C1_ROW/$C2_ROW/$C3_ROW): $LINE"
fi
capture /output/pm-05-jump-failure.png
DIFF=$(window_diff /output/pm-03-bottom.png /output/pm-05-jump-failure.png)
if [ "${DIFF:-0}" -lt "$VIEWPORT_DIFF_MIN" ]; then
    fail "PHASE 5 FAIL: the failure jump changed $DIFF px (min $VIEWPORT_DIFF_MIN)"
fi
echo "PHASE 5 PASS: ctrl+shift+b skipped the two successful commands and landed on the failure at row $FAIL_TARGET (+$DIFF px)"

# ── Phase 6: a server ScrollBottom snaps the viewport ─────────────
# The server emits `ScrollBottom` when it suppresses an AI session's ED 3, so
# the pane is armed as an AI session (OSC 1337 `ScribeAiLaunch`) and then emits
# a real ED 3. Both go out in one printf, which puts the arming event in the
# same PTY chunk the filter inspects.
send_keys shift+Prior
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) fail "PHASE 6 FAIL: could not scroll away from the bottom first: $LINE" ;;
esac
BASE=$(count_log "server snapped the pane to the live bottom")
scribe-test send "$SESSION" \
    "printf '\033]1337;ScribeAiLaunch=claude_code\007\033[3J'; echo PM_ED3_SENT\n"
scribe-test wait-output "$SESSION" "PM_ED3_SENT" --timeout 20000
if ! wait_for_log_growth "server snapped the pane to the live bottom" "$BASE" 20; then
    fail "PHASE 6 FAIL: the server's ScrollBottom never reached the client"
fi
LINE=$(last_log_line "server snapped the pane to the live bottom")
case "$LINE" in
    *"moved=true"*) ;;
    *) fail "PHASE 6 FAIL: the snap did not move a scrolled-up viewport: $LINE" ;;
esac
# The viewport is only genuinely at the bottom if an explicit scroll_bottom now
# has nothing left to do.
BASE=$(count_log "terminal scrollback moved")
send_keys shift+End
if ! wait_for_log_growth "terminal scrollback moved" "$BASE"; then
    fail "PHASE 6 FAIL: shift+End never reached scroll_bottom"
fi
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"moved=false"*"offset=0"*) ;;
    *) fail "PHASE 6 FAIL: the server snap left the viewport off the live bottom: $LINE" ;;
esac
capture /output/pm-06-scroll-bottom.png
echo "PHASE 6 PASS: the server's ScrollBottom snapped the view to the live bottom — $LINE"

echo ""
echo "PASS: visual prompt-marks test"
echo "  Inspect screenshots in test-output/:"
echo "    pm-00-attached.png       — the shared pane before any input"
echo "    pm-01-no-marks.png       — after jump_to_failure with no marks"
echo "    pm-02-marks-emitted.png  — after three OSC 133 command records"
echo "    pm-03-bottom.png         — the live bottom before any jump"
echo "    pm-03-jump-up-1.png      — parked on the newest prompt mark"
echo "    pm-03-jump-up-2.png      — parked one mark further up"
echo "    pm-04-jump-down.png      — back down one mark"
echo "    pm-05-jump-failure.png   — parked on the failed command"
echo "    pm-06-scroll-bottom.png  — after the server's ScrollBottom snap"
