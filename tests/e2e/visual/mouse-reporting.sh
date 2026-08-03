#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted + visual E2E: the mouse wheel and xterm mouse reporting in the
# running GPUI client.
#
# `mouse_reporting.rs` shipped with a green golden-byte suite and no caller at
# all, and the crate contained no scroll-wheel handling whatsoever — the 016
# reachability audit recorded it as an unwired module for exactly that reason. A
# headless test cannot tell a wired encoder from an unwired one, so every
# assertion here comes from the real window, the real wire, or the real PTY:
#
#   * the wheel over the grid moves the client's own viewport (a client log line
#     the wired path alone writes, plus a whole-viewport pixel diff);
#   * once an application in the pane enables mouse tracking, the same wheel and
#     the same clicks leave the client as `KeyInput` frames whose bytes the wire
#     tap records — and `cat -v` inside the pane prints those bytes back, so the
#     daemon's own screen snapshot proves they reached the PTY;
#   * the SGR-1006 and X10 encodings, the modifier bits, the Shift override and
#     the tracking-off fallback are each asserted on those recorded bytes.
#
# The window is driven through XTEST (`xdotool click`, no `--window`): GPUI reads
# input through XInput2 and ignores the synthetic XSendEvent input that
# `xdotool --window` delivers, so window-targeted input would leave the client
# untouched while the script still "passed". Buttons 4 and 5 are the wheel.
#
# Requires the shared-pane rig plus the wire tap:
#   just e2e-visual-mouse-reporting
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the shared-pane rig must export a created SESSION}"

# Lit pixels the grid must hold before any phase runs, measured the way
# terminal-viewport.sh measures it: an unattached window reads a few hundred, a
# live pane showing a prompt reads thousands.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Differing pixels a whole-viewport repaint must produce. Paging through the
# scrollback replaces every text row; a swallowed wheel event leaves consecutive
# frames byte-identical (the image pins SCRIBE_DISABLE_ANIMATIONS=1).
VIEWPORT_DIFF_MIN="${VIEWPORT_DIFF_MIN:-5000}"

POLL_TICKS="${POLL_TICKS:-20}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -20 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "FAIL: no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.5
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
    scrot -o /output/mouse-fullscreen.png
    convert /output/mouse-fullscreen.png \
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

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# Wait until the client log holds more copies of a pattern than `baseline`.
wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" now started
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# The most recent client log line containing $1, with the tracing formatter's
# ANSI colour codes stripped — they sit between the field name and its value
# (`rows\e[0m=\e[0m3`), so an unstripped line matches no `field=value` test.
last_log_line() {
    grep -F "$1" "$CLIENT_LOG" 2>/dev/null | tail -1 | sed -e 's/\x1b\[[0-9;]*m//g'
}

# Move the pointer into the terminal grid. $1/$2 are offsets from the window
# origin; the default lands well inside the grid band, clear of the tab strip on
# top and the status bands underneath.
point_at() {
    xdotool mousemove --sync $(( WIN_X + ${1:-200} )) $(( WIN_Y + ${2:-$(( WIN_H / 2 ))} ))
    sleep 0.2
}

# Click a button (1 left, 2 middle, 3 right, 4 wheel-up, 5 wheel-down).
click() {
    xdotool click "$1"
    sleep 0.5
}

# ── Wire-tap readers ──────────────────────────────────────────────
# Every KeyInput this session put on the wire, one escaped payload per line
# (ESC rendered as \x1b, exactly as the client logs it).
wire_key_inputs() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
try:
    handle = open(path)
except OSError:
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "KeyInput":
            continue
        if str(message.get("session_id")) != session:
            continue
        data = message.get("data") or []
        print("".join(
            chr(b) if 0x20 <= b <= 0x7e else "\\x%02x" % b
            for b in data
        ))
PY
}

# How many recorded KeyInput payloads match an extended regex.
count_wire_matches() {
    wire_key_inputs | grep -cE "$1" || true
}

# The newest recorded KeyInput payload matching an extended regex.
last_wire_match() {
    wire_key_inputs | grep -E "$1" | tail -1
}

# Wait until another KeyInput matching $1 lands on the wire.
wait_for_wire() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_wire_matches "$pattern")" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# ── PTY reader ────────────────────────────────────────────────────
# `scribe-test snapshot` writes the screen as a JSON cell grid, so the plain
# text a phase wants to grep has to be rebuilt from it row by row.
#
# The daemon answers a snapshot request from its cached `latest_snapshot` and
# only *then* replaces it with the one it just asked the server for, so any
# single call returns the previous screen. Every read here therefore polls until
# the expected text shows up, which is also what makes it a wait rather than a
# race against the PTY round trip.
pty_text() {
    scribe-test snapshot "$SESSION" "$1" >/dev/null
    python3 - "$1" <<'PY'
import json, sys

with open(sys.argv[1]) as handle:
    screen = json.load(handle)
cols = screen.get("cols") or 1
cells = [cell.get("c", " ") for cell in screen.get("cells", [])]
for start in range(0, len(cells), cols):
    print("".join(cells[start:start + cols]).rstrip())
PY
}

# Poll the pane's screen until it holds the fixed string $1, leaving the last
# read in $2 (JSON) and $3 (text). Returns non-zero on timeout.
wait_for_pty_text() {
    local needle="$1" json="$2" text="$3" timeout_secs="${4:-20}" started
    started=$(date +%s)
    while true; do
        pty_text "$json" >"$text"
        if grep -qF "$needle" "$text"; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.5
    done
}

# ── Pane drivers ──────────────────────────────────────────────────
# Put the pane back at a shell prompt: interrupt whatever is reading, restore
# the line discipline, and let the prompt redraw.
reset_pane() {
    scribe-test send "$SESSION" '\x03'
    sleep 0.5
    scribe-test send "$SESSION" 'stty sane\n'
    sleep 0.8
}

# Run `cat -v` in the pane behind the DEC private modes in $1, so mouse reports
# arriving at the PTY are printed back as visible text (`^[[<0;12;5M`). The
# non-canonical, non-echoing line discipline is what makes them appear the
# instant they arrive and exactly once.
track_with() {
    reset_pane
    scribe-test send "$SESSION" "stty -icanon -echo min 1 time 0; printf '$1'; cat -v\n"
    sleep 1.5
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/mouse-00-attached.png
    ink=$(window_ink /output/mouse-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: fill the scrollback there is nothing to scroll without ──
scribe-test send "$SESSION" 'for i in $(seq 1 200); do echo "mouse-line-$i"; done; printf "MOUSE_%s\n" FILLED\n'
scribe-test wait-output "$SESSION" "MOUSE_FILLED" --timeout 20000 >/dev/null \
    || fail "PHASE 1 FAIL: the seeded rows never reached the session"
sleep 1.0
capture /output/mouse-01-bottom.png
echo "PHASE 1 PASS: 200 scrollback rows emitted and echoed back"

# ── Phase 2: the wheel scrolls the client's own viewport ──────────
# `mouse wheel` is written only by the new scroll-wheel listener, and
# `terminal scrollback moved` only by `scroll_terminal`. Both plus the pixel
# diff are required: the log alone does not prove the frame changed, and the
# diff alone does not prove which code produced it.
focus
point_at
BASE_WHEEL=$(count_log "mouse wheel")
BASE_SCROLL=$(count_log "terminal scrollback moved")
click 4
if ! wait_for_log_growth "mouse wheel" "$BASE_WHEEL"; then
    fail "PHASE 2 FAIL: the wheel reached no listener at all (still unwired?)"
fi
LINE=$(last_log_line "mouse wheel")
case "$LINE" in
    *"action=Scrollback"*) ;;
    *) fail "PHASE 2 FAIL: a plain shell prompt did not route the wheel to the scrollback: $LINE" ;;
esac
case "$LINE" in
    *"rows=3"*) ;;
    *) fail "PHASE 2 FAIL: one wheel notch was not worth three rows: $LINE" ;;
esac
if ! wait_for_log_growth "terminal scrollback moved" "$BASE_SCROLL"; then
    fail "PHASE 2 FAIL: the wheel never reached scroll_terminal"
fi
# Three more notches, so the viewport is unmistakably inside the scrollback.
click 4
click 4
click 4
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) fail "PHASE 2 FAIL: the display offset stayed at the live bottom: $LINE" ;;
esac
capture /output/mouse-02-scrolled.png
DIFF=$(window_diff /output/mouse-01-bottom.png /output/mouse-02-scrolled.png)
if [ "${DIFF:-0}" -lt "$VIEWPORT_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: the wheel changed $DIFF px (min $VIEWPORT_DIFF_MIN); the viewport never moved"
fi
echo "PHASE 2 PASS: wheel-up paged into the scrollback (+$DIFF px) — $LINE"

# ── Phase 3: wheel-down returns to the live bottom ────────────────
for _ in $(seq 1 6); do
    click 5
done
LINE=$(last_log_line "terminal scrollback moved")
case "$LINE" in
    *"offset=0"*) ;;
    *) fail "PHASE 3 FAIL: wheel-down did not walk back to the live bottom: $LINE" ;;
esac
capture /output/mouse-03-back-at-bottom.png
echo "PHASE 3 PASS: wheel-down returned the viewport to the live bottom — $LINE"

# ── Phase 4: an application enables SGR-1006 mouse tracking ───────
# `cat -v` behind DECSET 1000 + 1006 makes the PTY the third, independent
# oracle: every byte the client forwards is printed straight back onto the pane
# the daemon can snapshot.
track_with '\033[?1000h\033[?1006h'
point_at
BASE_WHEEL=$(count_log "mouse wheel")
click 4
if ! wait_for_log_growth "mouse wheel" "$BASE_WHEEL"; then
    fail "PHASE 4 FAIL: the wheel stopped reaching the listener"
fi
LINE=$(last_log_line "mouse wheel")
case "$LINE" in
    *"action=Report"*) ;;
    *) fail "PHASE 4 FAIL: the client never saw the application's DECSET 1000: $LINE" ;;
esac
echo "PHASE 4 PASS: the pane's application took the pointer — $LINE"

# ── Phase 5: the wheel is reported as button 64 / 65 ──────────────
SGR_UP='\\x1b\[<64;[0-9]+;[0-9]+M'
SGR_DOWN='\\x1b\[<65;[0-9]+;[0-9]+M'
BASE=$(count_wire_matches "$SGR_UP")
click 4
if ! wait_for_wire "$SGR_UP" "$BASE"; then
    fail "PHASE 5 FAIL: wheel-up put no SGR button-64 report on the wire"
fi
UP_REPORT=$(last_wire_match "$SGR_UP")
BASE=$(count_wire_matches "$SGR_DOWN")
click 5
if ! wait_for_wire "$SGR_DOWN" "$BASE"; then
    fail "PHASE 5 FAIL: wheel-down put no SGR button-65 report on the wire"
fi
DOWN_REPORT=$(last_wire_match "$SGR_DOWN")
# The viewport must NOT have moved: a tracking application owns the wheel.
BASE_SCROLL=$(count_log "terminal scrollback moved")
click 4
if wait_for_log_growth "terminal scrollback moved" "$BASE_SCROLL" 3; then
    fail "PHASE 5 FAIL: the wheel scrolled the client's viewport while the application was tracking"
fi
# `cat -v` prints ESC as `^[`, so the reports appear on the pane's own screen
# exactly as the encoders produced them. This is the third oracle and the only
# one that proves the bytes completed the client → server → PTY round trip.
if ! wait_for_pty_text '^[[<64;' /output/mouse-05-pty.json /output/mouse-05-pty.txt; then
    fail "PHASE 5 FAIL: the button-64 report never reached the PTY (see /output/mouse-05-pty.txt)"
fi
if ! grep -qF '^[[<65;' /output/mouse-05-pty.txt; then
    fail "PHASE 5 FAIL: the button-65 report never reached the PTY (see /output/mouse-05-pty.txt)"
fi
capture /output/mouse-05-wheel-reports.png
echo "PHASE 5 PASS: the wheel is reported byte-identically — up '$UP_REPORT', down '$DOWN_REPORT'"

# ── Phase 6: presses and releases carry the button and the cell ───
SGR_PRESS='\\x1b\[<0;[0-9]+;[0-9]+M'
SGR_RELEASE='\\x1b\[<0;[0-9]+;[0-9]+m'
BASE_PRESS=$(count_wire_matches "$SGR_PRESS")
BASE_RELEASE=$(count_wire_matches "$SGR_RELEASE")
point_at 90
click 1
if ! wait_for_wire "$SGR_PRESS" "$BASE_PRESS"; then
    fail "PHASE 6 FAIL: a left click put no SGR press report on the wire"
fi
if ! wait_for_wire "$SGR_RELEASE" "$BASE_RELEASE"; then
    fail "PHASE 6 FAIL: a left click put no SGR release report on the wire"
fi
LEFT_NEAR=$(last_wire_match "$SGR_PRESS")
# A click further right must report a different column, which is what separates
# a wired encoder from one handed a constant.
BASE_PRESS=$(count_wire_matches "$SGR_PRESS")
point_at 420
click 1
if ! wait_for_wire "$SGR_PRESS" "$BASE_PRESS"; then
    fail "PHASE 6 FAIL: the second left click put no press report on the wire"
fi
LEFT_FAR=$(last_wire_match "$SGR_PRESS")
if [ "$LEFT_NEAR" = "$LEFT_FAR" ]; then
    fail "PHASE 6 FAIL: two clicks 330 px apart reported the same cell ($LEFT_NEAR)"
fi
# The right button is Cb 2, and Ctrl adds 16 — the modifier bits are part of
# "byte-identical", so both are asserted rather than assumed.
BASE=$(count_wire_matches '\\x1b\[<2;[0-9]+;[0-9]+M')
click 3
if ! wait_for_wire '\\x1b\[<2;[0-9]+;[0-9]+M' "$BASE"; then
    fail "PHASE 6 FAIL: a right click did not report button 2"
fi
BASE=$(count_wire_matches '\\x1b\[<16;[0-9]+;[0-9]+M')
xdotool keydown ctrl
click 1
xdotool keyup ctrl
sleep 0.3
if ! wait_for_wire '\\x1b\[<16;[0-9]+;[0-9]+M' "$BASE"; then
    fail "PHASE 6 FAIL: ctrl+click did not add the +16 modifier bit"
fi
capture /output/mouse-06-button-reports.png
echo "PHASE 6 PASS: presses and releases carry the button, the cell, and the modifier bits"

# ── Phase 7: Shift takes the pointer back from the application ────
# Holding Shift must reach NO encoder at all, so the user can still select text
# inside a mouse-tracking program.
BEFORE=$(count_log "mouse input forwarded")
xdotool keydown shift
point_at 250
click 1
xdotool keyup shift
sleep 0.8
AFTER=$(count_log "mouse input forwarded")
if [ "$AFTER" -ne "$BEFORE" ]; then
    fail "PHASE 7 FAIL: shift+click still forwarded $(( AFTER - BEFORE )) report(s) to the application"
fi
echo "PHASE 7 PASS: Shift held the pointer back from the tracking application"

# ── Phase 8: the legacy X10 encoding ──────────────────────────────
# DECRST 1006 drops back to the pre-SGR wire form, whose press report is six
# raw bytes rather than a printable CSI.
track_with '\033[?1000h\033[?1006l'
# `ESC [ M` then three bytes: Cb 0 (left, no modifiers) offset by 32 is a
# space, and the two coordinate bytes are printable at any sane cell.
X10_PRESS='\\x1b\[M [ -~][ -~]'
BASE=$(count_wire_matches "$X10_PRESS")
point_at 150
click 1
if ! wait_for_wire "$X10_PRESS" "$BASE"; then
    fail "PHASE 8 FAIL: with SGR off, the click did not fall back to the X10 encoding"
fi
X10_REPORT=$(last_wire_match "$X10_PRESS")
if ! wait_for_pty_text '^[[M' /output/mouse-08-pty.json /output/mouse-08-pty.txt; then
    fail "PHASE 8 FAIL: the X10 report never reached the PTY (see /output/mouse-08-pty.txt)"
fi
capture /output/mouse-08-x10.png
echo "PHASE 8 PASS: the X10 fallback is on the wire — '$X10_REPORT'"

# ── Phase 9: tracking off gives the wheel back to the viewport ────
track_with '\033[?1000l\033[?1006l'
point_at
BASE_WHEEL=$(count_log "mouse wheel")
BASE_SCROLL=$(count_log "terminal scrollback moved")
click 4
if ! wait_for_log_growth "mouse wheel" "$BASE_WHEEL"; then
    fail "PHASE 9 FAIL: the wheel reached no listener after DECRST 1000"
fi
LINE=$(last_log_line "mouse wheel")
case "$LINE" in
    *"action=Scrollback"*) ;;
    *) fail "PHASE 9 FAIL: DECRST 1000 did not return the wheel to the viewport: $LINE" ;;
esac
if ! wait_for_log_growth "terminal scrollback moved" "$BASE_SCROLL"; then
    fail "PHASE 9 FAIL: the wheel never moved the viewport again"
fi
capture /output/mouse-09-tracking-off.png
echo "PHASE 9 PASS: DECRST 1000 gave the wheel back to the scrollback — $LINE"

reset_pane

echo ""
echo "ALL PHASES PASS — the wheel scrolls and mouse reporting is on the wire."
echo "  Captures:    test-output/mouse-0*.png"
echo "  Wire record: test-output/share-wire.jsonl"
echo "  PTY echo:    test-output/mouse-05-pty.txt, test-output/mouse-08-pty.txt"
