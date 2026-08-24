#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the terminal viewport surfaces are reachable in the real client.
#
# Five modules landed with green unit suites and no caller — `split_scroll`,
# `vi_mode`, `smart_selection`, `zoom`, and the scrollback half of the terminal
# snapshot. A `#[gpui::test]` over any of them proves the math and says nothing
# about whether the running binary can reach it, which is exactly the gap the
# 016 reachability audit found. So every phase here drives the real window and
# asserts something only the wired path can produce.
#
# The window is driven through XTEST (`xdotool key`, no `--window`): GPUI reads
# the keyboard through XInput2 and ignores the synthetic XSendEvent input that
# `xdotool --window` delivers, so window-targeted input would leave the client
# untouched while the script still "passed".
#
# Requires the shared-pane rig:
#   just e2e-visual-terminal-viewport
# which exports SESSION and starts with `terminal.scroll_pin = false`.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
# The hook channel's endpoint IS the server socket; `scribe-hook-helper` needs
# no daemon, only the path and a session id in its environment.
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"

# Lit pixels the grid must hold before any phase runs, measured the same way
# color-emoji.sh measures it: an unattached window reads a few hundred, a live
# pane showing only a prompt reads thousands.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Differing pixels a whole-viewport repaint must produce. Paging up replaces
# every text row; a swallowed chord leaves consecutive frames byte-identical
# (the image pins SCRIBE_DISABLE_ANIMATIONS=1).
VIEWPORT_DIFF_MIN="${VIEWPORT_DIFF_MIN:-5000}"

# Differing pixels the vi cursor must add. It is one hollow cell box, so the
# floor is deliberately small — but a swallowed chord still yields exactly 0.
CURSOR_DIFF_MIN="${CURSOR_DIFF_MIN:-8}"

POLL_TICKS="${POLL_TICKS:-20}"

# Bands excluded from `grid_diff`: the integrated tab strip on top and the
# status line plus the system-stats bar underneath, all of which repaint on
# timers of their own.
GRID_TOP_INSET_PX="${GRID_TOP_INSET_PX:-40}"
GRID_BOTTOM_INSET_PX="${GRID_BOTTOM_INSET_PX:-80}"

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
    scrot -o /output/viewport-fullscreen.png
    convert /output/viewport-fullscreen.png \
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

# Differing pixels inside the terminal grid only. The status bar's sparklines
# resample on a timer, so a whole-window diff carries tens of pixels of noise
# that would swamp a small assertion like the one-cell vi cursor.
grid_diff() {
    local h value
    h=$(( WIN_H - GRID_TOP_INSET_PX - GRID_BOTTOM_INSET_PX ))
    value=$(compare -metric AE \
        \( "$1" -crop "${WIN_W}x${h}+0+${GRID_TOP_INSET_PX}" +repage \) \
        \( "$2" -crop "${WIN_W}x${h}+0+${GRID_TOP_INSET_PX}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

set_scroll_pin() {
    local enabled="$1" baseline
    baseline=$(count_log "config hot-reloaded")
    printf '[terminal]\nscroll_pin = %s\n' "$enabled" >"$CONFIG_FILE"
    wait_for_log_growth "config hot-reloaded" "$baseline" \
        || fail "FAIL: scroll_pin=$enabled did not hot-reload"
    sleep 0.3
}

# Locate the only 30px foreground component in the pane's lower-right control
# band, then click its measured centre. This follows the painted grid instead
# of guessing how X11 window decorations affect client-space coordinates.
jump_to_bottom() {
    local x y w h
    local -a controls
    capture /output/vp-jump-target.png
    mapfile -t controls < <(convert /output/vp-jump-target.png \
    -crop "60x60+$(( WIN_W - 80 ))+$(( WIN_H - 110 ))" +repage \
    -fuzz 3% -transparent '#0e0e10' -alpha extract -threshold 0 \
    -define connected-components:verbose=true -connected-components 8 null: 2>&1 \
        | awk '$NF != "gray(0)" {
            geometry = $2
            gsub(/[x+]/, " ", geometry)
            split(geometry, part)
            if (part[1] >= 28 && part[1] <= 32 && part[2] >= 28 && part[2] <= 32)
                print part[3], part[4], part[1], part[2]
        }')
    [ "${#controls[@]}" -eq 1 ] \
        || fail "FAIL: expected one 30px jump control, found ${#controls[@]} (${controls[*]:-none})"
    read -r x y w h <<<"${controls[0]}"
    xdotool mousemove --sync \
        $(( WIN_X + WIN_W - 80 + x + w / 2 )) \
        $(( WIN_Y + WIN_H - 110 + y + h / 2 ))
    xdotool click 1
    sleep 0.5
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

# Post one provider hook event for $SESSION straight to the server, exactly as
# a real AI tool's hook adapter would.
hook() {
    SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper "$@"
    sleep 1.0
}

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -20 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/vp-00-attached.png
    ink=$(window_ink /output/vp-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: fill the scrollback there is nothing to scroll without ──
scribe-test send "$SESSION" 'for i in $(seq 1 200); do echo "vp-line-$i"; done; printf "VP_%s\n" FILLED\n'
scribe-test wait-output "$SESSION" "VP_FILLED" --timeout 20000
capture /output/vp-01-bottom.png
echo "PHASE 1 PASS: 200 scrollback rows emitted and echoed back"

# ── Phase 2: shift+PageUp pages the viewport into the scrollback ──
# `terminal scrollback moved` is written only by `scroll_terminal`, which only
# LayoutAction::Scroll* reaches; the pixel diff proves the snapshot honoured
# the new display offset instead of repainting the live screen.
BASE=$(count_log "terminal scrollback moved")
focus
send_keys shift+Prior
if ! wait_for_log_growth "terminal scrollback moved" "$BASE"; then
    fail "PHASE 2 FAIL: shift+PageUp never reached scroll_up (still swallowed?)"
fi
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) fail "PHASE 2 FAIL: the display offset stayed at the live bottom: $LINE" ;;
esac
capture /output/vp-02-scrolled.png
DIFF=$(window_diff /output/vp-01-bottom.png /output/vp-02-scrolled.png)
if [ "${DIFF:-0}" -lt "$VIEWPORT_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: paging up changed $DIFF px (min $VIEWPORT_DIFF_MIN); the snapshot ignored the offset"
fi
echo "PHASE 2 PASS: shift+PageUp paged into the scrollback (+$DIFF px) — $LINE"

# ── Phase 2b: one plain-pane click returns to the bottom ─────────
# No provider hook has run and scroll_pin is false, so this is the ordinary
# terminal path. The control claims the click before selection or a terminal
# application's mouse reporter can see it, then scrolls exactly this session.
BASE_JUMP=$(count_log "terminal jump-to-bottom clicked")
BASE_SCROLL=$(count_log "terminal scrollback moved")
focus
jump_to_bottom
if ! wait_for_log_growth "terminal jump-to-bottom clicked" "$BASE_JUMP"; then
    fail "PHASE 2b FAIL: the plain pane's jump control did not claim one click"
fi
if ! wait_for_log_growth "terminal scrollback moved" "$BASE_SCROLL"; then
    fail "PHASE 2b FAIL: the jump control did not route through scroll_session"
fi
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) ;;
    *) fail "PHASE 2b FAIL: the click left the pane scrolled: $LINE" ;;
esac
capture /output/vp-02b-jump-bottom.png
BASE_JUMP=$(count_log "terminal jump-to-bottom clicked")
jump_to_bottom
if wait_for_log_growth "terminal jump-to-bottom clicked" "$BASE_JUMP" 3; then
    fail "PHASE 2b FAIL: the at-bottom control stayed clickable"
fi
echo "PHASE 2b PASS: one plain-pane click returned to offset 0 and then hid"

# ── Phase 3: shift+End returns to the live bottom ─────────────────
BASE=$(count_log "terminal scrollback moved")
send_keys shift+End
if ! wait_for_log_growth "terminal scrollback moved" "$BASE"; then
    fail "PHASE 3 FAIL: shift+End never reached scroll_bottom"
fi
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) ;;
    *) fail "PHASE 3 FAIL: scroll_bottom left the viewport off the bottom: $LINE" ;;
esac
capture /output/vp-03-back-at-bottom.png
DIFF=$(window_diff /output/vp-01-bottom.png /output/vp-03-back-at-bottom.png)
echo "PHASE 3 PASS: shift+End restored the live bottom (${DIFF}px from the phase-1 frame)"

# ── Phase 5: vi / copy mode owns the keyboard ─────────────────────
# Three things have to hold: the chord toggles the mode, a motion key paints a
# cursor, and — the whole point of copy mode — the motion key does NOT reach
# the PTY. The last one is asserted against the daemon's own screen snapshot,
# which the client's additive attach leaves intact.
capture /output/vp-05-before-vi.png
BASE=$(count_log "vi mode toggled")
send_keys ctrl+shift+space
if ! wait_for_log_growth "vi mode toggled" "$BASE"; then
    fail "PHASE 5 FAIL: ctrl+shift+space never reached the vi-mode chord"
fi
LINE=$(last_log_line "vi mode toggled")
case "$LINE" in
    *"active=true"*) ;;
    *) fail "PHASE 5 FAIL: the chord did not enter vi mode: $LINE" ;;
esac
# Twelve rows up, not two: the vi cursor is seeded on the shell cursor at the
# very bottom of the grid, which sits inside the band `grid_diff` crops away.
VI_MOTIONS="${VI_MOTIONS:-12}"
for _ in $(seq 1 "$VI_MOTIONS"); do
    xdotool key --clearmodifiers k
done
sleep 0.6
capture /output/vp-05-vi-cursor.png
DIFF=$(grid_diff /output/vp-05-before-vi.png /output/vp-05-vi-cursor.png)
if [ "${DIFF:-0}" -lt "$CURSOR_DIFF_MIN" ]; then
    fail "PHASE 5 FAIL: vi mode painted no cursor ($DIFF px changed)"
fi
scribe-test snapshot "$SESSION" /output/vp-05-pty.txt
if grep -q "kkkk" /output/vp-05-pty.txt; then
    fail "PHASE 5 FAIL: the motion keys leaked into the shell (see /output/vp-05-pty.txt)"
fi
BASE=$(count_log "vi mode toggled")
send_keys Escape
if ! wait_for_log_growth "vi mode toggled" "$BASE"; then
    fail "PHASE 5 FAIL: Escape never left vi mode"
fi
LINE=$(last_log_line "vi mode toggled")
case "$LINE" in
    *"active=false"*) ;;
    *) fail "PHASE 5 FAIL: Escape did not leave vi mode: $LINE" ;;
esac
echo "PHASE 5 PASS: vi mode toggles, paints a cursor (+$DIFF px), and swallows its motions"

# ── Phase 6: split-scroll pins the live bottom in an AI pane ──────
# The pin needs both halves of the gate: `terminal.scroll_pin` (seeded by the
# rig's config) and a session the client believes is an AI pane, which only a
# real provider hook event can establish.
send_keys shift+End
set_scroll_pin true
hook --provider=claude_code --event=state_changed --state=processing
BASE=$(count_log "terminal scrollback moved")
send_keys shift+Prior
if ! wait_for_log_growth "terminal scrollback moved" "$BASE"; then
    fail "PHASE 6 FAIL: shift+PageUp never reached scroll_up"
fi
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"pin_rows=0"*) fail "PHASE 6 FAIL: split-scroll stayed off in an AI pane: $LINE" ;;
    *"pin_rows="*) ;;
    *) fail "PHASE 6 FAIL: no pin_rows reported: $LINE" ;;
esac
capture /output/vp-06-split-scroll.png
echo "PHASE 6 PASS: the live bottom is pinned while scrolled — $LINE"

# The pin exists so a prompt stays composable while reading scrollback, so an
# ordinary keystroke must NOT collapse it and Enter must. Both halves are
# asserted, because a pin that snaps on every key is indistinguishable from no
# pin at all.
BASE=$(count_log "snapped the viewport to the live bottom")
send_keys x
if wait_for_log_growth "snapped the viewport to the live bottom" "$BASE" 3; then
    fail "PHASE 6b FAIL: an ordinary keystroke collapsed the pin"
fi
send_keys Return
if ! wait_for_log_growth "snapped the viewport to the live bottom" "$BASE" 10; then
    fail "PHASE 6b FAIL: Enter did not snap the viewport back to the live bottom"
fi
LINE=$(last_log_line "snapped the viewport to the live bottom")
case "$LINE" in
    *"pinned=true"*) ;;
    *) fail "PHASE 6b FAIL: the snap did not come from a pinned viewport: $LINE" ;;
esac
capture /output/vp-06-after-enter.png
echo "PHASE 6b PASS: typing kept the pin up and Enter collapsed it — $LINE"

# ── Phase 7: a right-click resolves smart-selection rules ─────────
# Fill the viewport with URLs so any click in the left third of the grid lands
# inside one, then assert the client named the matching rule. `smart selection
# matched` is written only by `smart_selection_rows`, which only the live
# context-menu path calls.
send_keys shift+End
scribe-test send "$SESSION" 'for i in $(seq 1 60); do echo "https://example.com/spec/$i"; done; printf "VP_%s\n" URLS\n'
scribe-test wait-output "$SESSION" "VP_URLS" --timeout 20000
sleep 1.0
BASE=$(count_log "smart selection matched")
focus
xdotool mousemove --sync $(( WIN_X + 80 )) $(( WIN_Y + (WIN_H / 2) ))
xdotool click 3
sleep 0.6
if ! wait_for_log_growth "smart selection matched" "$BASE"; then
    fail "PHASE 7 FAIL: the right-click resolved no smart-selection rule"
fi
LINE=$(last_log_line "smart selection matched")
case "$LINE" in
    *"rule=URI"*) ;;
    *) fail "PHASE 7 FAIL: the URI rule did not win at the click point: $LINE" ;;
esac
capture /output/vp-07-context-menu.png
send_keys Escape
echo "PHASE 7 PASS: the right-click resolved a live smart-selection rule — $LINE"

echo ""
echo "PASS: visual terminal-viewport test"
echo "  Inspect screenshots in test-output/:"
echo "    vp-00-attached.png       — the shared pane before any input"
echo "    vp-01-bottom.png         — the live bottom after 200 rows"
echo "    vp-02-scrolled.png       — after shift+PageUp paged into scrollback"
echo "    vp-02b-jump-bottom.png  — plain-pane click returned to the live view"
echo "    vp-03-back-at-bottom.png — after shift+End returned to the live view"
echo "    vp-05-vi-cursor.png      — the vi cursor after twelve upward motions"
echo "    vp-06-split-scroll.png   — the pinned live bottom under scrollback"
echo "    vp-07-context-menu.png   — the menu carrying the matched URI rule"
