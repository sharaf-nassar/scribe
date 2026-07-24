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

# Minimum count of strongly-saturated pixels expected once color emoji render.
# The six solid color-block glyphs alone contribute hundreds of saturated
# pixels; a monochrome/tinted fallback tints them the pale foreground color and
# leaves this count at ~0, so a few hundred is a wide, stable margin against the
# full-screen scrot's near-grayscale background.
SATURATED_MIN_PIXELS="${SATURATED_MIN_PIXELS:-300}"

capture_window() {
    local out="$1"
    local wid
    wid=$(xdotool search --name "Scribe" | head -1) || true
    if [ -n "$wid" ]; then
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
        xdotool windowraise "$wid" 2>/dev/null || true
        sleep 0.5
    fi
    # Full-screen capture — Vulkan surfaces may not be readable per-window.
    scrot "$out"
}

# --- Phase 1: Emit a grid of color emoji ---
# Solid color-block emoji give unambiguous saturated fills; the pictographic
# ones exercise the multicolor glyph path.
scribe-test send "$SESSION" 'printf "EMOJI_GRID_BEGIN\\n"\n'
scribe-test wait-output "$SESSION" "EMOJI_GRID_BEGIN"
scribe-test send "$SESSION" 'printf "\xf0\x9f\x9f\xa5\xf0\x9f\x9f\xa7\xf0\x9f\x9f\xa8\xf0\x9f\x9f\xa9\xf0\x9f\x9f\xa6\xf0\x9f\x9f\xaa\\n"\n'
scribe-test send "$SESSION" 'printf "\xf0\x9f\x98\x80\xe2\x9d\xa4\xef\xb8\x8f\xf0\x9f\x91\x8d\xf0\x9f\x8c\x88\\n"\n'
scribe-test send "$SESSION" 'printf "EMOJI_GRID_END\\n"\n'
scribe-test wait-output "$SESSION" "EMOJI_GRID_END"
sleep 1
echo "PHASE 1 PASS: emoji grid emitted"

# --- Phase 2: Capture the frame ---
capture_window /output/emoji-grid.png
echo "PHASE 2 PASS: screenshot captured"

# --- Phase 3: Assert saturated (color) pixels are present ---
# Separate the HSL saturation channel and count pixels above a high threshold.
# %[fx:mean*w*h] over a thresholded (0/1) image yields the white-pixel count.
saturated=$(
    convert /output/emoji-grid.png -colorspace HSL -channel G -separate +channel \
        -threshold 60% -format "%[fx:mean*w*h]" info:
)
saturated=${saturated%.*}

echo "Saturated pixels: $saturated (min $SATURATED_MIN_PIXELS)"

pass=$(awk -v s="$saturated" -v m="$SATURATED_MIN_PIXELS" 'BEGIN { print (s >= m) ? 1 : 0 }')
if [ "$pass" != "1" ]; then
    echo "FAIL: emoji rendered without color — $saturated saturated pixels below $SATURATED_MIN_PIXELS" >&2
    echo "      (monochrome/tinted glyph fallback likely; see /output/emoji-grid.png)" >&2
    exit 1
fi

echo "PASS: color emoji rendered in color ($saturated saturated pixels)"
