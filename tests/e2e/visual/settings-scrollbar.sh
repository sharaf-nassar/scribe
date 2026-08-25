#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Visual E2E Tests#The settings scrollbar is a control]]
# Scripted E2E: the settings content pane's overlay scrollbar is a control.
#
# The settings pane painted a thumb and answered nothing: hover, press, drag and
# click-to-jump all belonged to the terminal pane only, so the page-length
# affordance was a hint the pointer could not touch. Wiring it reuses
# `crate::scrollbar` wholesale, and that module's `#[gpui::test]` suite already
# proves the geometry — `hit_test_scrollbar`, `hit_test_thumb`,
# `offset_from_track_click` and `offset_from_drag` all have unit tests.
#
# What a unit test cannot prove is reachability: that a real pointer press at a
# real X server lands on the settings window's own handler rather than on the
# control it is painted over, and — the inverse failure, which is worse — that
# the invisible overlay does NOT swallow presses on a page that fits. Both are
# window-level facts, so every phase below drives the real settings window and
# reads the thumb straight out of the pixels.
#
# The oracle is a narrow strip cropped from the content pane's right edge, the
# only region the overlay writes into: the rows are laid out inside the
# scroller's 44px padding, so no glyph ever reaches it. `probe_thumb` reports
# the bright-pixel box in that strip, which yields the thumb's pixel count, its
# painted width, its top edge, and its centre column. Every gesture below is
# then aimed at the thumb the previous capture actually found, so the script
# carries no layout constant for where the scrollbar sits.
#
# Input is driven through XTEST (`xdotool mousemove` / `click` / `mousedown`,
# and `mousemove --window` only for the window-relative warp). GPUI reads input
# through XInput2 and ignores the synthetic XSendEvent input `xdotool --window`
# delivers for keys and clicks, so window-targeted input would leave the window
# untouched while the script still "passed".
#
# The recipe deliberately passes SCRIBE_DISABLE_ANIMATIONS=0, unpinning the
# image's default kill switch. That switch flips GPUI's global reduce-motion
# flag, and `tick_content_scrollbar` honours it by pinning the thumb fully
# opaque and requesting no animation frames — so under the image default there
# is no idle fade to stop and no hover width lerp to observe. The fade and the
# widen ARE the behaviour under test here, so this one suite runs with motion
# on; nothing it asserts is a byte-identical frame comparison.
#
# Requires: visual container with SCRIBE_VISUAL_APP=settings, i.e.
#   just e2e-visual-settings-scrollbar
set -e

SETTINGS_LOG="${SCRIBE_SETTINGS_LOG:-/output/settings.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Lit pixels the settings window must paint, matching settings-entry.sh: a page
# of labelled rows over a dark ground is far more than this, an unpainted or
# blank window far less.
SETTINGS_INK_MIN="${SETTINGS_INK_MIN:-500}"

# Window-local geometry the phases need but cannot measure.
#
# `SETTINGS_TITLEBAR_HEIGHT` and the 232px sidebar are the two bands the strip
# and the sidebar crop are carved out of; both are settings/window.rs constants
# and a change to either fails a phase here rather than silently shifting a
# measurement onto the wrong pixels.
TITLEBAR_H=38
NAV_WIDTH=232

# Width of the right-edge strip the thumb is read out of. The thumb is 6px wide
# (9px hovered) inset 2px from the pane's right edge, so 32px is comfortably
# wider than the overlay in either state and still narrower than the scroller's
# 44px right padding, which is what keeps page content out of the strip.
STRIP_W="${STRIP_W:-32}"

# Mean channel value a strip pixel needs to count as thumb. The page ground is
# `page_bg` (#141518, mean ~22) and the thumb is a white wash at 24% alpha over
# it, which lands far above this in either blend space; a half-covered edge
# column still clears it.
THUMB_GRAY_MIN="${THUMB_GRAY_MIN:-45}"

# Strip pixels a painted thumb must hold. The resting thumb is 6px wide and at
# least 20px tall, so even the shortest possible thumb is over 100px.
THUMB_MIN_PIXELS="${THUMB_MIN_PIXELS:-100}"
# Strip pixels tolerated when the overlay is meant to be gone. Not zero, so a
# stray antialiased pixel is not a failure; far below one thumb.
THUMB_ABSENT_MAX="${THUMB_ABSENT_MAX:-20}"

# Extra painted columns hover must add. `HOVER_EXTRA_WIDTH` is 3px, so a
# converged widen reads 6 -> 9; requiring 2 leaves room for the antialiased
# edge column being counted on one capture and not the other.
WIDEN_MIN="${WIDEN_MIN:-2}"

# Pixels the thumb is dragged, and how far the thumb's own top edge may land
# from that. The drag is the inverse of the thumb placement, so the thumb
# tracks the pointer 1:1; the tolerance only absorbs the whole-pixel rounding
# in `round_scroll_units`.
DRAG_PX="${DRAG_PX:-140}"
DRAG_TOLERANCE="${DRAG_TOLERANCE:-30}"

# How far a track click must move the thumb. The click lands below the thumb
# and near the track bottom, which pins `display_offset` at 0 and puts the
# thumb at the far end of its travel — hundreds of pixels in this window.
JUMP_MIN="${JUMP_MIN:-80}"

# Content-pane pixels a scroll must repaint. A drag or a jump moves the page by
# most of a viewport, so this floor separates "the page scrolled" from "only
# the thumb moved", which is the whole point of asserting both.
CONTENT_CHANGE_MIN="${CONTENT_CHANGE_MIN:-1000}"

# Sidebar pixels the cleared focus ring must repaint. The ring is a 1px amber
# outline around a 216x32 nav row — roughly 490 perimeter pixels against a near
# black ground — so this floor is met several times over by a ring that clears
# and not at all by one that never did.
SIDEBAR_CHANGE_MIN="${SIDEBAR_CHANGE_MIN:-100}"

# `settings_nav_pages()` order, from settings/window.rs: Appearance, Colors,
# Terminal, Keybindings, AI, Environment, Workspaces, Updates, Notifications,
# Remote, Agent API. Focus traversal starts at index 0 with no visible focus,
# so N Down presses land on index N.
#
# Keybindings is the overflow page: 60 action rows are several viewports tall.
# Environment is the page that fits: two action rows under one heading.
DOWNS_TO_KEYBINDINGS=3
DOWNS_TO_ENVIRONMENT=5

WIN=""
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0
SEED_X=60
SEED_Y=0

find_window() {
    xdotool search --name '^Scribe Settings$' 2>/dev/null | tail -1
}

fail() {
    echo "$1"
    echo "--- settings log tail ---"
    tail -40 "$SETTINGS_LOG" 2>/dev/null || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" 2>/dev/null || true
    exit 1
}

raise_window() {
    if [ -z "$WIN" ]; then
        WIN=$(find_window)
    fi
    if [ -z "$WIN" ]; then
        fail "FAIL: no Scribe Settings window found"
    fi
    xdotool windowactivate --sync "$WIN" 2>/dev/null \
        || xdotool windowfocus --sync "$WIN" 2>/dev/null || true
    sleep 0.3
}

# Cache the window's CLIENT box. `xwininfo`, not `xdotool getwindowgeometry`:
# openbox reparents the window into a decorated frame and xdotool reports that
# frame, which would offset every crop by the decoration.
measure_window() {
    local info
    info=$(xwininfo -id "$WIN")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    WIN_H=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    SEED_Y=$(( WIN_H - 18 ))
    echo "settings window ${WIN_W}x${WIN_H} at ${WIN_X},${WIN_Y}"
}

shot() {
    sleep 0.35
    scrot -o "$1"
}

window_ink() {
    local value
    value=$(convert "$1" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Pixels that changed inside an arbitrary window-relative box. The small
# threshold drops antialiasing while retaining any real repaint.
changed_pixels() {
    local before="$1" after="$2" w="$3" h="$4" x="$5" y="$6" value
    value=$(convert "$before" "$after" -compose difference -composite \
        -crop "${w}x${h}+$(( WIN_X + x ))+$(( WIN_Y + y ))" +repage \
        -colorspace Gray -threshold 3% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Everything the content pane paints except the overlay strip itself, so a
# "the page scrolled" assertion can never be satisfied by the thumb moving.
content_changed() {
    changed_pixels "$1" "$2" \
        "$(( WIN_W - NAV_WIDTH - STRIP_W ))" "$(( WIN_H - TITLEBAR_H ))" \
        "$NAV_WIDTH" "$TITLEBAR_H"
}

sidebar_changed() {
    changed_pixels "$1" "$2" "$NAV_WIDTH" "$(( WIN_H - TITLEBAR_H ))" 0 "$TITLEBAR_H"
}

# The overlay thumb's bright-pixel box inside the right-edge strip, as
# "count width top centre_x" — count and width in pixels, top and centre_x in
# window-relative coordinates. Reports "0 0 0 0" when nothing is painted.
#
# `convert … txt:` plus sed rather than a gawk regex capture: the test image
# ships mawk, which has no three-argument `match`.
probe_thumb() {
    local png="$1" x0
    x0=$(( WIN_W - STRIP_W ))
    convert "$png" \
        -crop "${STRIP_W}x$(( WIN_H - TITLEBAR_H ))+$(( WIN_X + x0 ))+$(( WIN_Y + TITLEBAR_H ))" \
        +repage -depth 8 -alpha off txt:- \
        | sed -n 's/^\([0-9]*\),\([0-9]*\): (\([0-9]*\),\([0-9]*\),\([0-9]*\)).*/\1 \2 \3 \4 \5/p' \
        | awk -v min="$THUMB_GRAY_MIN" -v x0="$x0" -v top="$TITLEBAR_H" '
            ($3 + $4 + $5) / 3 >= min {
                n++
                if (n == 1 || $1 < lo) lo = $1
                if (n == 1 || $1 > hi) hi = $1
                if (n == 1 || $2 < ty) ty = $2
            }
            END {
                if (n == 0) { print "0 0 0 0"; exit }
                printf "%d %d %d %d\n", n, hi - lo + 1, top + ty, x0 + int((lo + hi) / 2)
            }'
}

key() {
    xdotool key --clearmodifiers "$@"
    sleep 0.3
}

press_down() {
    local times="$1" i
    for (( i = 0; i < times; i++ )); do
        xdotool key --clearmodifiers Down
        sleep 0.2
    done
    sleep 0.3
}

# Warp the pointer to a window-relative point. `mousemove --window` warps the
# real pointer (it is not the XSendEvent path), and under a reparenting WM it
# is the only form whose coordinates match the client box every crop uses.
point_at() {
    xdotool mousemove --window "$WIN" "$1" "$2"
    sleep 0.4
}

# Give the GPUI root its focus handle and reset traversal to its first target,
# by clicking the sidebar's inert version footer — the one part of the window
# that cannot become a control by accident (a `flex_none` `Role::Note` pinned
# below the nav list). It also parks the pointer far from the scrollbar.
reset_focus() {
    point_at "$SEED_X" "$SEED_Y"
    xdotool click 1
    sleep 0.5
}

# ── Phase 0: the settings window is up and painted ────────────────
raise_window
NAME=$(xdotool getwindowname "$WIN")
if [ "$NAME" != "Scribe Settings" ]; then
    fail "PHASE 0 FAIL: window $WIN is titled '$NAME', not 'Scribe Settings'"
fi
measure_window
reset_focus
shot /output/ss-00-open.png
INK=$(window_ink /output/ss-00-open.png)
if [ "$INK" -lt "$SETTINGS_INK_MIN" ]; then
    fail "PHASE 0 FAIL: the settings window painted $INK px (min $SETTINGS_INK_MIN)"
fi
echo "PHASE 0 PASS: the settings window is up and painted ($INK lit px)"

# ── Phase 1: an overflowing page paints a thumb, then fades it ────
# Selecting a page rewinds the scroller and pulses the overlay, so the thumb is
# on screen for the 1.5s idle delay plus the 0.3s ramp. Two captures inside
# that window, because the first can land before the new page has painted; the
# richer one is the resting measurement every later phase is compared against.
press_down "$DOWNS_TO_KEYBINDINGS"
xdotool key --clearmodifiers Return
sleep 0.35
scrot -o /output/ss-01-thumb-a.png
sleep 0.4
scrot -o /output/ss-01-thumb-b.png
read -r REST_N REST_W _ REST_CX <<<"$(probe_thumb /output/ss-01-thumb-a.png)"
read -r ALT_N ALT_W _ ALT_CX <<<"$(probe_thumb /output/ss-01-thumb-b.png)"
if [ "$ALT_N" -gt "$REST_N" ]; then
    REST_N=$ALT_N REST_W=$ALT_W REST_CX=$ALT_CX
    cp /output/ss-01-thumb-b.png /output/ss-01-thumb.png
else
    cp /output/ss-01-thumb-a.png /output/ss-01-thumb.png
fi
if [ "$REST_N" -lt "$THUMB_MIN_PIXELS" ]; then
    fail "PHASE 1 FAIL: the overflowing page painted ${REST_N}px of thumb (min $THUMB_MIN_PIXELS)"
fi
sleep 2.5
shot /output/ss-01-faded.png
read -r FADED_N _ _ _ <<<"$(probe_thumb /output/ss-01-faded.png)"
if [ "$FADED_N" -gt "$THUMB_ABSENT_MAX" ]; then
    fail "PHASE 1 FAIL: the idle overlay never faded (${FADED_N}px still in the strip)"
fi
echo "PHASE 1 PASS: the thumb painted ${REST_N}px at ${REST_W}px wide, then faded to ${FADED_N}px"

# ── Phase 2: pointer entry widens the thumb and stops the fade ────
# The hit zone is three times the thumb width, so the resting thumb's own
# centre column is inside it whichever state the width lerp is in. Holding the
# pointer there past the idle delay is what separates "hover repainted it once"
# from "hover pinned it open".
point_at "$REST_CX" "$(( TITLEBAR_H + (WIN_H - TITLEBAR_H) / 2 ))"
sleep 1.2
shot /output/ss-02-hover.png
read -r HOVER_N HOVER_W HOVER_TOP HOVER_CX <<<"$(probe_thumb /output/ss-02-hover.png)"
if [ "$HOVER_N" -lt "$THUMB_MIN_PIXELS" ]; then
    fail "PHASE 2 FAIL: hovering the hit zone brought back ${HOVER_N}px of thumb (min $THUMB_MIN_PIXELS)"
fi
if [ "$HOVER_W" -lt "$(( REST_W + WIDEN_MIN ))" ]; then
    fail "PHASE 2 FAIL: the thumb painted ${HOVER_W}px wide hovered against ${REST_W}px resting (min +$WIDEN_MIN)"
fi
sleep 2.5
shot /output/ss-02-hover-held.png
read -r HELD_N _ _ _ <<<"$(probe_thumb /output/ss-02-hover-held.png)"
if [ "$HELD_N" -lt "$THUMB_MIN_PIXELS" ]; then
    fail "PHASE 2 FAIL: the hovered overlay faded out anyway (${HELD_N}px left in the strip)"
fi
echo "PHASE 2 PASS: hover widened the thumb ${REST_W}px -> ${HOVER_W}px and held it open (${HELD_N}px after 2.5s idle)"

# ── Phase 3: a thumb drag scrolls the page with the pointer ───────
# The page is still rewound to the top, so the thumb sits at the track top and
# a press 8px into it is a press on the thumb for any thumb height (the floor
# is 20px). Dragging down must move the thumb down with the pointer AND repaint
# the page: the thumb alone would prove the overlay, not the scroll.
DRAG_FROM=$(( HOVER_TOP + 8 ))
DRAG_TO=$(( DRAG_FROM + DRAG_PX ))
point_at "$HOVER_CX" "$DRAG_FROM"
shot /output/ss-03-before-drag.png
xdotool mousedown 1
sleep 0.3
point_at "$HOVER_CX" "$DRAG_TO"
shot /output/ss-03-dragged.png
read -r DRAG_N _ DRAG_TOP _ <<<"$(probe_thumb /output/ss-03-dragged.png)"
if [ "$DRAG_N" -lt "$THUMB_MIN_PIXELS" ]; then
    xdotool mouseup 1
    fail "PHASE 3 FAIL: the thumb vanished mid-drag (${DRAG_N}px in the strip)"
fi
MOVED=$(( DRAG_TOP - HOVER_TOP ))
if [ "$MOVED" -lt "$(( DRAG_PX - DRAG_TOLERANCE ))" ] || [ "$MOVED" -gt "$(( DRAG_PX + DRAG_TOLERANCE ))" ]; then
    xdotool mouseup 1
    fail "PHASE 3 FAIL: the pointer dragged ${DRAG_PX}px down and the thumb moved ${MOVED}px (tolerance $DRAG_TOLERANCE)"
fi
DRAG_CONTENT=$(content_changed /output/ss-03-before-drag.png /output/ss-03-dragged.png)
if [ "$DRAG_CONTENT" -lt "$CONTENT_CHANGE_MIN" ]; then
    xdotool mouseup 1
    fail "PHASE 3 FAIL: the drag repainted ${DRAG_CONTENT} content px (min $CONTENT_CHANGE_MIN); the thumb moved without the page"
fi
echo "PHASE 3 PASS: a ${DRAG_PX}px drag moved the thumb ${MOVED}px down and repainted ${DRAG_CONTENT} content px"

# ── Phase 4: the overlay does not fade out mid-drag ───────────────
# The drag holds the button past the idle delay plus the fade ramp. Press start
# clears the fade timer for exactly this reason: without it the thumb would
# dissolve under a pointer that is still holding it.
sleep 2.5
shot /output/ss-04-drag-held.png
read -r HOLD_N _ _ _ <<<"$(probe_thumb /output/ss-04-drag-held.png)"
xdotool mouseup 1
sleep 0.4
if [ "$HOLD_N" -lt "$THUMB_MIN_PIXELS" ]; then
    fail "PHASE 4 FAIL: the overlay faded to ${HOLD_N}px while the drag was still held"
fi
echo "PHASE 4 PASS: the thumb held ${HOLD_N}px through a 2.5s pause mid-drag"

# ── Phase 5: a track click jumps the viewport ─────────────────────
# The click lands below the thumb, near the bottom of the track, which resolves
# to `display_offset` 0 — the live end of the page — and moves the thumb to the
# far end of its travel.
shot /output/ss-05-before-jump.png
point_at "$HOVER_CX" "$(( WIN_H - 24 ))"
xdotool click 1
sleep 0.6
shot /output/ss-05-jumped.png
read -r JUMP_N _ JUMP_TOP _ <<<"$(probe_thumb /output/ss-05-jumped.png)"
if [ "$JUMP_N" -lt "$THUMB_MIN_PIXELS" ]; then
    fail "PHASE 5 FAIL: no thumb after the track click (${JUMP_N}px in the strip)"
fi
JUMPED=$(( JUMP_TOP - DRAG_TOP ))
if [ "$JUMPED" -lt "$JUMP_MIN" ]; then
    fail "PHASE 5 FAIL: the track click moved the thumb ${JUMPED}px (min $JUMP_MIN)"
fi
JUMP_CONTENT=$(content_changed /output/ss-05-before-jump.png /output/ss-05-jumped.png)
if [ "$JUMP_CONTENT" -lt "$CONTENT_CHANGE_MIN" ]; then
    fail "PHASE 5 FAIL: the track click repainted ${JUMP_CONTENT} content px (min $CONTENT_CHANGE_MIN)"
fi
echo "PHASE 5 PASS: the track click jumped the thumb ${JUMPED}px and repainted ${JUMP_CONTENT} content px"

# ── Phase 6: a page that fits keeps the press ─────────────────────
# The regression guard for the failure that makes an overlay dangerous: on a
# page with nothing to scroll there is no thumb and no track, so the hit zone
# must not exist and the press must reach what it was painted over.
#
# The press is observed through the keyboard focus ring. Traversal leaves an
# amber outline on the selected nav row; the window root clears it on any left
# press, and the scrollbar's capture-phase handler stops propagation whenever
# it consumes one. So a ring that clears is a press that reached the root, and
# a ring that survives is a press the invisible overlay ate.
reset_focus
press_down "$DOWNS_TO_ENVIRONMENT"
key Return
point_at "$HOVER_CX" "$(( TITLEBAR_H + (WIN_H - TITLEBAR_H) / 2 ))"
sleep 1.2
shot /output/ss-06-short-page.png
read -r SHORT_N _ _ _ <<<"$(probe_thumb /output/ss-06-short-page.png)"
if [ "$SHORT_N" -gt "$THUMB_ABSENT_MAX" ]; then
    fail "PHASE 6 FAIL: a page that fits painted ${SHORT_N}px of thumb"
fi
xdotool click 1
sleep 0.6
shot /output/ss-06-after-press.png
RING=$(sidebar_changed /output/ss-06-short-page.png /output/ss-06-after-press.png)
if [ "$RING" -lt "$SIDEBAR_CHANGE_MIN" ]; then
    fail "PHASE 6 FAIL: the press changed ${RING} sidebar px (min $SIDEBAR_CHANGE_MIN); the overlay swallowed it"
fi
echo "PHASE 6 PASS: the page that fits painted no thumb and the press reached the window underneath ($RING sidebar px)"

echo ""
echo "PASS: visual settings-scrollbar test"
echo "  Inspect screenshots in test-output/:"
echo "    ss-00-open.png         — the settings window on open"
echo "    ss-01-thumb.png        — the thumb on an overflowing page"
echo "    ss-01-faded.png        — the same page after the idle fade"
echo "    ss-02-hover.png        — the widened thumb under the pointer"
echo "    ss-02-hover-held.png   — hover holding the overlay past the idle delay"
echo "    ss-03-dragged.png      — the page dragged down by the thumb"
echo "    ss-04-drag-held.png    — the overlay still painted mid-drag"
echo "    ss-05-jumped.png       — the viewport after a track click"
echo "    ss-06-short-page.png   — a page that fits, with no overlay at all"
echo "    ss-06-after-press.png  — the press that reached the window underneath"
