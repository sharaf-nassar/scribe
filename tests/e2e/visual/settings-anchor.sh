#!/usr/bin/env bash
# The settings window opens centred on the Scribe window that asked for it.
#
# This is a placement test, not a pixel test: X11 treats creation bounds as a
# hint, so a window that computes the right position can still be mapped in the
# corner of the screen by the window manager. It therefore measures where the
# window actually landed, which is the only thing that can catch that.
set -uo pipefail

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

TERM_WIN=""
for _ in $(seq 1 60); do
    TERM_WIN=$(xdotool search --name '^Scribe$' 2>/dev/null | head -1)
    [ -n "$TERM_WIN" ] && break
    sleep 0.5
done
[ -n "$TERM_WIN" ] || fail "no Scribe terminal window found"

xdotool windowactivate "$TERM_WIN" >/dev/null 2>&1
sleep 1

# Put the terminal somewhere that is not the corner, so "centred on the parent"
# and "placed by the window manager" cannot look alike.
xdotool windowmove "$TERM_WIN" 260 180
xdotool windowsize "$TERM_WIN" 900 600
sleep 1.5

eval "$(xdotool getwindowgeometry --shell "$TERM_WIN" | sed 's/^/TERM_/')"
TERM_CX=$(( TERM_X + TERM_WIDTH / 2 ))
TERM_CY=$(( TERM_Y + TERM_HEIGHT / 2 ))
echo "terminal ${TERM_WIDTH}x${TERM_HEIGHT} at ${TERM_X},${TERM_Y} (centre ${TERM_CX},${TERM_CY})"

# Open settings with the real chord.
xdotool key --window "$TERM_WIN" ctrl+comma
SET_WIN=""
for _ in $(seq 1 60); do
    SET_WIN=$(xdotool search --name '^Scribe Settings$' 2>/dev/null | head -1)
    [ -n "$SET_WIN" ] && break
    sleep 0.5
done
[ -n "$SET_WIN" ] || fail "the settings chord opened no settings window"
sleep 3

eval "$(xdotool getwindowgeometry --shell "$SET_WIN" | sed 's/^/SET_/')"
SET_CX=$(( SET_X + SET_WIDTH / 2 ))
SET_CY=$(( SET_Y + SET_HEIGHT / 2 ))
echo "settings ${SET_WIDTH}x${SET_HEIGHT} at ${SET_X},${SET_Y} (centre ${SET_CX},${SET_CY})"

scrot -o /output/anchor-01-opened.png

DX=$(( SET_CX > TERM_CX ? SET_CX - TERM_CX : TERM_CX - SET_CX ))
DY=$(( SET_CY > TERM_CY ? SET_CY - TERM_CY : TERM_CY - SET_CY ))
echo "centre offset ${DX},${DY}"

# A window manager that ignored the position drops the window at the screen
# corner, which is hundreds of pixels out. Frame borders and work-area clamping
# are worth a modest tolerance, nothing more.
[ "$DX" -le 60 ] || fail "settings centre is ${DX}px off the terminal's on X"
[ "$DY" -le 60 ] || fail "settings centre is ${DY}px off the terminal's on Y"

# The chord raises an already-open settings window rather than stacking a
# duplicate — and it has to bring it back over the window that asked. Move the
# terminal with settings still open, press the chord again, and the window must
# follow rather than stay where it was left (on another monitor, in the report
# this covers).
xdotool windowmove "$TERM_WIN" 620 300
sleep 1.5
eval "$(xdotool getwindowgeometry --shell "$TERM_WIN" | sed 's/^/MOVED_/')"
MOVED_CX=$(( MOVED_X + MOVED_WIDTH / 2 ))
MOVED_CY=$(( MOVED_Y + MOVED_HEIGHT / 2 ))

xdotool windowactivate "$TERM_WIN" >/dev/null 2>&1
sleep 0.5
xdotool key --window "$TERM_WIN" ctrl+comma
SET2=$(xdotool search --name '^Scribe Settings$' 2>/dev/null | head -1)
[ -n "$SET2" ] || fail "the settings window vanished on the second chord"
sleep 3
eval "$(xdotool getwindowgeometry --shell "$SET2" | sed 's/^/S2_/')"
S2_CX=$(( S2_X + S2_WIDTH / 2 ))
S2_CY=$(( S2_Y + S2_HEIGHT / 2 ))
scrot -o /output/anchor-02-reopened.png
DX2=$(( S2_CX > MOVED_CX ? S2_CX - MOVED_CX : MOVED_CX - S2_CX ))
DY2=$(( S2_CY > MOVED_CY ? S2_CY - MOVED_CY : MOVED_CY - S2_CY ))
echo "reopen centre offset ${DX2},${DY2} (terminal centre ${MOVED_CX},${MOVED_CY})"
[ "$DX2" -le 60 ] || fail "reopened settings is ${DX2}px off the moved terminal on X"
[ "$DY2" -le 60 ] || fail "reopened settings is ${DY2}px off the moved terminal on Y"

echo "PASS: settings opened centred on its terminal (offset ${DX},${DY}; after move ${DX2},${DY2})"
