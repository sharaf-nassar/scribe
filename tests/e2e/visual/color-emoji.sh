#!/bin/bash
set -e

# Visual E2E: assert the GPUI client renders color emoji in color, not as
# monochrome/tinted glyphs in the foreground text color. This is the US3
# headline parity item promoted to an automated visual check.
#
# Strategy: print a row of solid-fill color-block emoji (each a single
# saturated hue) plus a few pictographic emoji, screenshot the window, and
# measure how many strongly saturated pixels the frame contains. A color
# rendering yields many saturated pixels (red/green/blue blocks); a monochrome
# fallback tints every glyph the pale status/foreground color and stays
# near-grayscale, so the saturated-pixel count collapses.
#
# Three things make that measurement mean what it says, and all three were
# missing while this script reported a pass over an empty black grid:
#
#   * the shared-pane rig (SCRIBE_SHARED_PANE=1, see docker/entrypoint-visual.sh)
#     attaches the client to the very session `scribe-test` drives, so the emoji
#     this script sends actually reach the window under the camera;
#   * every measurement is cropped to the client window. A full-screen scrot
#     also catches openbox's light-blue title bar, which on its own clears any
#     saturated-pixel threshold — the frame could be entirely black and the
#     count would still read ~980;
#   * a lit-pixel phase fails FIRST, and separately, when the grid holds no pane
#     content, so "no color" can never be reported for what is really "no pane".
#
# Requires: visual container run with SCRIBE_SHARED_PANE=1
# (`just e2e-visual-shared visual/color-emoji.sh`), which exports SESSION.

# Minimum count of strongly-saturated pixels expected once color emoji render.
# The six solid color-block glyphs alone contribute hundreds of saturated
# pixels; a monochrome/tinted fallback tints them the pale foreground color and
# leaves this count at ~0, so a few hundred is a wide, stable margin against the
# cropped grid's near-black background.
SATURATED_MIN_PIXELS="${SATURATED_MIN_PIXELS:-300}"

# Minimum lit pixels the client's GRID must hold before any color claim is
# believable, and the bottom band excluded from that count.
#
# Both numbers are measured, not guessed. The status bar is a bright full-width
# strip that is lit whether or not a pane is attached and contributes ~5 500 lit
# pixels on its own, so it is cropped away first. What remains of an UNATTACHED
# window is ~670 px of window edge; a freshly attached pane showing only the
# shell prompt reads ~2 550, and the same pane after the emoji lines ~11 300.
# The floor has to sit between the first two, since this phase runs BEFORE
# anything is printed: 1 500 is ~2.3x the empty grid and ~1.7x under the
# quietest live one.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# Poll budget for each of the two "wait for a frame" phases. A tick is one
# focus + capture + measure + 0.5 s sleep, roughly 1.4 s, so 20 ticks give each
# phase ~28 s and leave both inside the entrypoint's 60 s TEST_TIMEOUT even when
# the first paint is slow. The happy path takes one or two ticks.
POLL_TICKS="${POLL_TICKS:-20}"

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

# Focus the client and cache its on-screen geometry so the full-screen capture
# can be cropped down to just the client window.
focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found" >&2
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    xdotool windowraise "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

capture_window() {
    local out="$1"
    focus
    sleep 0.3
    # Full-screen capture — Vulkan surfaces may not be readable per-window —
    # cropped to the client window so the WM title bar cannot be mistaken for
    # rendered content.
    scrot -o /output/emoji-grid-fullscreen.png
    convert /output/emoji-grid-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$out"
}

# Lit (non-background) pixels in the client's grid: rendered text is near-white
# on a near-black background, so a luminance threshold separates ink from pane.
# The status-bar band is dropped first — it is lit whether or not a pane is
# attached, and counting it would let an empty grid clear any floor.
window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Strongly saturated pixels in the cropped window. Separate the HSL saturation
# channel and count pixels above a high threshold; %[fx:mean*w*h] over a
# thresholded (0/1) image yields the white-pixel count.
window_saturation() {
    local value
    value=$(convert "$1" -colorspace HSL -channel G -separate +channel \
        -threshold 60% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# --- Phase 1: Wait for the client to paint the attached pane ---
# The entrypoint gates on the client's "attaching to session" line, which it
# logs when it SENDS `AttachSessions`; the replay, the first GPUI frame, and the
# software-Vulkan swapchain all still lie ahead, and under lavapipe that takes
# seconds. Polling here rather than sleeping is what keeps the color assertion
# below from racing a not-yet-painted window and blaming the glyph path for it.
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture_window /output/emoji-grid.png
    ink=$(window_ink /output/emoji-grid.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    echo "FAIL: the client grid rendered no pane content ($ink lit px, min $INK_MIN_PIXELS)" >&2
    echo "      (the client never attached to \$SESSION; see /output/emoji-grid.png)" >&2
    exit 1
fi
echo "PHASE 1 PASS: the client grid renders the attached pane ($ink lit px)"

# --- Phase 2: Emit a grid of color emoji ---
# Solid color-block emoji give unambiguous saturated fills; the pictographic
# ones exercise the multicolor glyph path. `wait-output` reads the daemon's copy
# of the PTY stream, which the client's additive attach leaves intact — the
# whole point of the shared-pane rig.
scribe-test send "$SESSION" 'printf "EMOJI_GRID_BEGIN\\n"\n'
scribe-test wait-output "$SESSION" "EMOJI_GRID_BEGIN"
scribe-test send "$SESSION" 'printf "\xf0\x9f\x9f\xa5\xf0\x9f\x9f\xa7\xf0\x9f\x9f\xa8\xf0\x9f\x9f\xa9\xf0\x9f\x9f\xa6\xf0\x9f\x9f\xaa\\n"\n'
scribe-test send "$SESSION" 'printf "\xf0\x9f\x98\x80\xe2\x9d\xa4\xef\xb8\x8f\xf0\x9f\x91\x8d\xf0\x9f\x8c\x88\\n"\n'
scribe-test send "$SESSION" 'printf "EMOJI_GRID_END\\n"\n'
scribe-test wait-output "$SESSION" "EMOJI_GRID_END"
echo "PHASE 2 PASS: emoji grid emitted and echoed back through scribe-test"

# --- Phase 3: Assert saturated (color) pixels are present ---
# Polled for the same reason as phase 1: the bytes are on the PTY, but the frame
# carrying them may be one repaint away.
saturated=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture_window /output/emoji-grid.png
    saturated=$(window_saturation /output/emoji-grid.png)
    [ "$saturated" -ge "$SATURATED_MIN_PIXELS" ] && break
    sleep 0.5
done
echo "Saturated pixels: $saturated (min $SATURATED_MIN_PIXELS)"
if [ "$saturated" -lt "$SATURATED_MIN_PIXELS" ]; then
    echo "FAIL: emoji rendered without color — $saturated saturated pixels below $SATURATED_MIN_PIXELS" >&2
    echo "      (monochrome/tinted glyph fallback likely; see /output/emoji-grid.png)" >&2
    exit 1
fi

echo "PASS: color emoji rendered in color ($saturated saturated pixels)"
