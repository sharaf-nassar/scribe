#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E: every chrome band is on screen, and the whole terminal grid fits
# above them.
#
# The window used to open at a hardcoded 960x680 — exactly the painted height of
# the 36-row grid (36 x 18.9 px) and nothing more. The titlebar, the pane status
# strip and the window status bar are stacked in the same flex column, so those
# 84 px came out of the grid: the bottom five rows were clipped, and a window
# only slightly shorter would have squeezed the bands themselves away because
# they were flex-shrinkable under a flex-grown grid.
#
# The window size is now derived (see
# `crates/scribe-client/src/window_chrome.rs`) and every band is
# `flex_none`. This test measures that on the running client:
#
#   * the window opens at the derived size and sits entirely on the screen;
#   * the grid viewport is tall enough for all ROWS rows;
#   * the grid's LAST row carries ink after the pane is filled;
#   * the pane status strip and the window status bar both carry ink, in their
#     own bands at the bottom of the window;
#   * a real `PromptReceived` hook makes the prompt strip appear between the
#     grid and the status strip WITHOUT pushing either band off screen.
#
# Every crop is taken from `import -window`, which captures the client window's
# own pixels, so all offsets below are window-relative and no WM decoration can
# shift them.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1 (the client must be
# attached to the pane `scribe-test send` writes to), xdotool, imagemagick.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"

# Layout constants, mirrored from the client so a drift shows up as a failure
# here rather than as silently clipped pixels:
#   TITLEBAR_HEIGHT      titlebar.rs
#   STATUS_STRIP_HEIGHT  window_chrome.rs
#   STATUS_BAR_HEIGHT    window_chrome.rs
#   COLUMNS / ROWS       main.rs
#   line height / cell width  terminal_element.rs at the default font size 14
#                             (14 * 1.35 = 18.9 and 14 * 0.6 = 8.4)
TITLEBAR_H=34
STRIP_H=26
BAR_H=24
ROWS=36
COLUMNS=120
# ceil(36 * 18.9) and ceil(120 * 8.4)
GRID_H_MIN=681
EXPECTED_W=1008
EXPECTED_H=$(( TITLEBAR_H + GRID_H_MIN + STRIP_H + BAR_H ))
# Painted row height x10, so the last row's top edge can be computed in integer
# arithmetic (14 * 1.35 = 18.9 px). ROW_CROP_H stays under it so the crop cannot
# spill into the band below.
ROW_H_X10=189
ROW_CROP_H=18

# A band this dark is empty. Rendered text is near-white over a near-black grid
# (14,14,16) or a near-black chrome band (29,29,31), so a luminance threshold
# separates ink from either background.
INK_MIN=40

# The prompt the hook channel raises, and the smallest repaint that counts as
# "the prompt strip replaced the grid rows that were in that band". Swapping a
# row of terminal text for the strip's own background, icon, text and timer
# moves thousands of pixels; with SCRIBE_DISABLE_ANIMATIONS=1 an unchanged band
# is byte-identical, so this only has to clear sampling noise.
PROMPT_TEXT="chrome band probe prompt"
PROMPT_DELTA_MIN="${PROMPT_DELTA_MIN:-200}"

WID=""
WIN_W=0
WIN_H=0

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    WID=$(find_window)
    [ -z "$WID" ] && fail "no Scribe window found"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.3
}

# Capture just the client window's own pixels.
shot() {
    focus
    sleep 0.3
    import -window "$WID" +repage "$1"
    echo "captured $1"
}

# Lit pixels in a window-relative band: $1 image, $2 y, $3 height.
band_ink() {
    local value
    value=$(convert "$1" -crop "${WIN_W}x${3}+0+${2}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Write a window-relative band to its own file: $1 src, $2 dest, $3 y, $4 height.
crop_band() {
    convert "$1" -crop "${WIN_W}x${4}+0+${3}" +repage "$2"
}

# Differing pixels between two same-size crops.
band_delta() {
    local diff
    diff=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    echo "${diff%%.*}"
}

# ── Phase 1: the window opens at the derived size, fully on screen ────────────
focus
eval "$(xdotool getwindowgeometry --shell "$WID")"
WIN_W="$WIDTH"
WIN_H="$HEIGHT"
echo "window geometry: ${WIN_W}x${WIN_H} at ${X},${Y}"

[ "$WIN_W" -eq "$EXPECTED_W" ] \
    || fail "window width $WIN_W != $EXPECTED_W (COLUMNS=$COLUMNS at 8.4 px/cell)"
[ "$WIN_H" -eq "$EXPECTED_H" ] \
    || fail "window height $WIN_H != $EXPECTED_H (titlebar+grid+strip+bar)"

GRID_Y="$TITLEBAR_H"
GRID_H=$(( WIN_H - TITLEBAR_H - STRIP_H - BAR_H ))
[ "$GRID_H" -ge "$GRID_H_MIN" ] \
    || fail "grid viewport $GRID_H px cannot show $ROWS rows (needs $GRID_H_MIN)"

# The whole client area has to be on the screen, not just requested: a window
# taller than the display would move the status bar off the *screen* instead of
# off the window, which is the same defect one level up.
read -r SCREEN_W SCREEN_H <<<"$(xdotool getdisplaygeometry)"
[ "$WIN_W" -le "$SCREEN_W" ] && [ "$WIN_H" -le "$SCREEN_H" ] \
    || fail "window ${WIN_W}x${WIN_H} does not fit the ${SCREEN_W}x${SCREEN_H} screen"
scrot -o /output/chrome-bands-screen.png
read -r FRAME_H <<<"$(convert /output/chrome-bands-screen.png \
    -bordercolor black -fuzz 1% -trim -format "%h" info:)"
[ "${FRAME_H:-0}" -ge "$WIN_H" ] \
    || fail "only ${FRAME_H}px of the ${WIN_H}px window is on screen"
echo "PHASE 1 PASS: ${WIN_W}x${WIN_H} window, ${GRID_H}px grid viewport, all on screen"

# ── Phase 2: fill the grid, then prove its LAST row is on screen ──────────────
# `seq 1 40` overflows a 36-row grid, so the bottom rows are guaranteed to hold
# scrolled-in output rather than blank cells below a short command.
STRIP_Y=$(( WIN_H - STRIP_H - BAR_H ))
BAR_Y=$(( WIN_H - BAR_H ))
LAST_ROW_Y=$(( GRID_Y + (ROWS - 1) * ROW_H_X10 / 10 ))

shot /output/chrome-bands-00-empty.png
BEFORE_LAST_ROW=$(band_ink /output/chrome-bands-00-empty.png "$LAST_ROW_Y" "$ROW_CROP_H")

scribe-test send "$SESSION" 'clear; seq 1 40; echo GRID_FILL_DONE\n'
scribe-test wait-output "$SESSION" "GRID_FILL_DONE"
sleep 1.0
shot /output/chrome-bands-01-filled.png

LAST_ROW_INK=$(band_ink /output/chrome-bands-01-filled.png "$LAST_ROW_Y" "$ROW_CROP_H")
echo "  last grid row ink: $BEFORE_LAST_ROW -> $LAST_ROW_INK"
[ "${LAST_ROW_INK:-0}" -ge "$INK_MIN" ] \
    || fail "grid row $ROWS (y=$LAST_ROW_Y) is blank: the grid is still clipped"
echo "PHASE 2 PASS: grid row $ROWS renders inside the window"

# ── Phase 3: both status bands carry ink, in their own bands ──────────────────
STRIP_INK=$(band_ink /output/chrome-bands-01-filled.png "$STRIP_Y" "$STRIP_H")
BAR_INK=$(band_ink /output/chrome-bands-01-filled.png "$BAR_Y" "$BAR_H")
echo "  status strip ink: $STRIP_INK   status bar ink: $BAR_INK"
[ "${STRIP_INK:-0}" -ge "$INK_MIN" ] \
    || fail "the pane status strip (y=$STRIP_Y) is not on screen"
[ "${BAR_INK:-0}" -ge "$INK_MIN" ] \
    || fail "the window status bar (y=$BAR_Y) is not on screen"
echo "PHASE 3 PASS: status strip and status bar both render at the window bottom"

# ── Phase 4: a real prompt makes the prompt strip appear, bands survive ───────
# One prompt row is max(cell_height + 10, 28) = 28 px (prompt_bar.rs), taken out
# of the flex-grown grid — the bands below it must not move off screen.
PROMPT_H=28
PROMPT_Y=$(( STRIP_Y - PROMPT_H ))
crop_band /output/chrome-bands-01-filled.png /output/chrome-bands-prompt-before.png \
    "$PROMPT_Y" "$PROMPT_H"

SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper \
    --provider=claude_code --event=prompt_received --text="$PROMPT_TEXT"
sleep 1.5
shot /output/chrome-bands-02-prompt.png
crop_band /output/chrome-bands-02-prompt.png /output/chrome-bands-prompt-after.png \
    "$PROMPT_Y" "$PROMPT_H"

PROMPT_INK=$(band_ink /output/chrome-bands-02-prompt.png "$PROMPT_Y" "$PROMPT_H")
PROMPT_DELTA=$(band_delta /output/chrome-bands-prompt-before.png \
    /output/chrome-bands-prompt-after.png)
STRIP_AFTER=$(band_ink /output/chrome-bands-02-prompt.png "$STRIP_Y" "$STRIP_H")
BAR_AFTER=$(band_ink /output/chrome-bands-02-prompt.png "$BAR_Y" "$BAR_H")
echo "  prompt band ink: $PROMPT_INK   repaint delta: $PROMPT_DELTA"
echo "  strip ink after prompt: $STRIP_AFTER   bar ink after prompt: $BAR_AFTER"
# The band under the grid was grid content before the prompt arrived, so ink
# alone proves nothing: the band has to have REPAINTED, and the client has to
# have logged the notice that made it repaint.
[ "${PROMPT_INK:-0}" -ge "$INK_MIN" ] \
    || fail "the prompt strip (y=$PROMPT_Y) never rendered"
[ "${PROMPT_DELTA:-0}" -ge "$PROMPT_DELTA_MIN" ] \
    || fail "the band at y=$PROMPT_Y did not repaint: no prompt strip appeared"
[ "${STRIP_AFTER:-0}" -ge "$INK_MIN" ] \
    || fail "the prompt strip pushed the pane status strip off screen"
[ "${BAR_AFTER:-0}" -ge "$INK_MIN" ] \
    || fail "the prompt strip pushed the window status bar off screen"
echo "PHASE 4 PASS: prompt strip renders above both status bands, none pushed off"

echo ""
echo "PASS: window chrome bands are all on screen at the default window size"
echo "  Inspect screenshots in test-output/:"
echo "    chrome-bands-00-empty.png   — window at rest"
echo "    chrome-bands-01-filled.png  — 36-row grid + both status bands"
echo "    chrome-bands-02-prompt.png  — prompt strip added, bands still on screen"
