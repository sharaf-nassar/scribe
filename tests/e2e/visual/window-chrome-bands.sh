#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E: configured tab/status geometry is live in one client process,
# and startup still leaves room for the whole terminal grid and every chrome
# band.
#
# The run starts at tab_height=16 / tab_bar_padding=0 / status_bar_height=8.
# It first raises only the status band to 48, then changes each tab input and
# measures the matching painted rows. The lower row reserves its configured
# height, pane content and hit testing begin below it, and the status controls
# remain inside their configured 8px band.
#
# Every capture uses `import -window`, so offsets are client-window-relative and
# no WM decoration can move a crop. Animations, cursor blink, and all status
# stats are disabled by the startup config so a zero ImageMagick AE delta really
# means the running client ignored the edit.
set -euo pipefail

CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"

STATUS_BAR_H=8
TALL_STATUS_BAR_H=48
ROWS=36
COLUMNS=120
GRID_H_MIN=681
EXPECTED_W=1008
START_TAB_H=16
START_PADDING=0
START_BAR_H=$(( START_TAB_H + START_PADDING ))
HEIGHT_BAR_H=60
PADDED_BAR_H=80
EXPECTED_H=$(( START_BAR_H + GRID_H_MIN + STATUS_BAR_H ))
ROW_H_X10=189
ROW_CROP_H=18
INK_MIN=40
PROMPT_TEXT="chrome band probe prompt"
PROMPT_DELTA_MIN="${PROMPT_DELTA_MIN:-200}"

WID=""
WIN_W=0
WIN_H=0
WIN_X=0
WIN_Y=0
PUBLISH_GROWTH=0

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" 2>/dev/null >&2 || true
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
    [ -n "$WID" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$WID")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

shot() {
    focus
    sleep 0.3
    import -window "$WID" +repage "$1"
    echo "captured $1"
}

count_log() {
    grep -cF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        [ "$now" -gt "$baseline" ] && return 0
        if [ $(( $(date +%s) - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.2
    done
}

write_geometry() {
    local tab_height="$1" padding="$2" status_height="$3"
    cat >"$CONFIG_FILE" <<EOF
[appearance]
animations = false
cursor_blink = false
tab_height = $tab_height
tab_bar_padding = $padding
status_bar_height = $status_height

[terminal.status_bar_stats]
cpu = false
gpu = false
memory = false
network = false

[remote]
sharing_mode = "free_for_all"
EOF
}

reload_geometry() {
    local tab_height="$1" padding="$2" status_height="$3" label="$4"
    local reloads_before publishes_before publishes_after
    reloads_before=$(count_log "config hot-reloaded")
    publishes_before=$(count_log "published a pane's grid size")
    write_geometry "$tab_height" "$padding" "$status_height"
    wait_for_log_growth "config hot-reloaded" "$reloads_before" 15 \
        || fail "$label edit never hot-reloaded"
    # Do not fail here: the pre-fix proof must reach both screenshot deltas and
    # report them together. The post-fix assertions below still require each
    # edit to republish pane geometry.
    wait_for_log_growth "published a pane's grid size" "$publishes_before" 5 || true
    publishes_after=$(count_log "published a pane's grid size")
    PUBLISH_GROWTH=$(( publishes_after - publishes_before ))
    sleep 0.5
}

# Measure the bottom of the chrome marks that differ from the stable pane
# background sampled at the end of this scan. Titlebar and grid backgrounds can
# be identical; the active underline and bottom hairline still end exactly at
# the row boundary. The right edge carries no terminal glyph ink below them.
measure_bar_height() {
    local image="$1" top="$2" scan_h="$3" label="$4"
    local x target_y target end mask
    x=$(( WIN_W - 4 ))
    target_y=$(( top + scan_h - 2 ))
    target=$(convert "$image" -format "%[pixel:p{$x,$target_y}]" info:)
    mask="/tmp/${label}-boundary-mask.png"
    convert "$image" -crop "1x${scan_h}+${x}+${top}" +repage -alpha on \
        -fuzz 2% -transparent "$target" "$mask" >/dev/null
    end=$(convert "$mask" -trim -format '%[fx:page.y+h]' info: 2>/dev/null || true)
    [ -n "$end" ] || fail "$label row boundary was not measurable"
    printf '%s' "${end%.*}"
}

crop_top() {
    convert "$1" -crop "${WIN_W}x100+0+0" +repage "$2"
}

crop_bottom() {
    convert "$1" -crop "${WIN_W}x100+0+$(( WIN_H - 100 ))" +repage "$2"
}

# Measure the status band against the stable grid background at the right edge.
# The top hairline starts the distinct run and the sampled terminal column has
# no glyph ink there, so its offset is the border-box band boundary.
measure_status_height() {
    local image="$1" top="$2" scan_h="$3" label="$4"
    local x target target_y mask start
    x=$(( WIN_W - 4 ))
    target_y=$(( top + 2 ))
    target=$(convert "$image" -format "%[pixel:p{$x,$target_y}]" info:)
    mask="/tmp/${label}-status-mask.png"
    convert "$image" -crop "1x${scan_h}+${x}+${top}" +repage -alpha on \
        -fuzz 2% -transparent "$target" "$mask" >/dev/null
    start=$(convert "$mask" -trim -format '%[fx:page.y]' info: 2>/dev/null || true)
    [ -n "$start" ] || fail "$label status boundary was not measurable"
    printf '%s' "$(( scan_h - ${start%.*} ))"
}

last_published_rows() {
    grep -F "published a pane's grid size" "$CLIENT_LOG" 2>/dev/null | tail -1 \
        | sed -E $'s/\033\\[[0-9;]*[[:alpha:]]//g' \
        | sed -nE 's/.*rows=([0-9]+).*/\1/p'
}

image_delta() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

band_ink() {
    local image="$1" y="$2" height="$3" value
    value=$(convert "$image" -crop "${WIN_W}x${height}+0+${y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

crop_band() {
    convert "$1" -crop "${WIN_W}x${4}+0+${3}" +repage "$2"
}

first_red_row() {
    local image="$1" x="$2" top="$3" height="$4" mask offset
    mask=/tmp/lower-pane-red-mask.png
    convert "$image" -crop "1x${height}+${x}+${top}" +repage -alpha off \
        -fx '(r > 0.3 && r > g * 1.25 && r > b * 1.25) ? 1 : 0' "$mask" >/dev/null
    offset=$(convert "$mask" -trim -format '%[fx:page.y]' info: 2>/dev/null || true)
    [ -n "$offset" ] || fail "lower pane red content was not measurable"
    printf '%s' "$(( top + ${offset%.*} ))"
}

# ── Phase 1: status geometry hot-reloads in the existing process ─────────────
sleep 0.8
focus
INITIAL_W="$WIN_W"
INITIAL_H="$WIN_H"
INITIAL_PID=$(pgrep -f '(^|/)scribe-client$' | head -1)
[ -n "$INITIAL_PID" ] || fail "no running scribe-client process"

shot /output/chrome-status-00-height-8.png
crop_bottom /output/chrome-status-00-height-8.png /output/chrome-status-00-height-8-bottom.png
STATUS_BASELINE_H=$(measure_status_height \
    /output/chrome-status-00-height-8.png "$(( WIN_H - 100 ))" 100 status-8)
STATUS_ROWS_BEFORE=$(last_published_rows)
[ -n "$STATUS_ROWS_BEFORE" ] || fail "baseline pane rows were not published"

reload_geometry "$START_TAB_H" "$START_PADDING" "$TALL_STATUS_BAR_H" "status_bar_height"
STATUS_PUBLISH_GROWTH="$PUBLISH_GROWTH"
shot /output/chrome-status-01-height-48.png
crop_bottom /output/chrome-status-01-height-48.png /output/chrome-status-01-height-48-bottom.png
STATUS_TALL_H=$(measure_status_height \
    /output/chrome-status-01-height-48.png "$(( WIN_H - 100 ))" 100 status-48)
STATUS_DELTA=$(image_delta \
    /output/chrome-status-00-height-8-bottom.png /output/chrome-status-01-height-48-bottom.png)
STATUS_ROWS_AFTER=$(last_published_rows)

CURRENT_PID=$(pgrep -f '(^|/)scribe-client$' | head -1)
[ "$CURRENT_PID" = "$INITIAL_PID" ] \
    || fail "client restarted during status geometry reload ($INITIAL_PID -> $CURRENT_PID)"
[ "${STATUS_DELTA:-0}" -gt 0 ] \
    || fail "status geometry stayed fixed (status_bar_height AE=$STATUS_DELTA)"
[ "$STATUS_BASELINE_H" -eq "$STATUS_BAR_H" ] \
    || fail "status_bar_height=8 measured ${STATUS_BASELINE_H}px"
[ "$STATUS_TALL_H" -eq "$TALL_STATUS_BAR_H" ] \
    || fail "status_bar_height=48 measured ${STATUS_TALL_H}px"
[ "$STATUS_PUBLISH_GROWTH" -gt 0 ] \
    || fail "status_bar_height edit did not republish pane geometry"
[ -n "$STATUS_ROWS_AFTER" ] && [ "$STATUS_ROWS_AFTER" -lt "$STATUS_ROWS_BEFORE" ] \
    || fail "status height growth did not reduce published rows ($STATUS_ROWS_BEFORE -> $STATUS_ROWS_AFTER)"
echo "PHASE 1 PASS: status band hot-reloaded 8 -> 48px in pid $INITIAL_PID (AE $STATUS_DELTA, rows $STATUS_ROWS_BEFORE -> $STATUS_ROWS_AFTER)"

# ── Phase 2: tab geometry remains live with the compact status band ───────────
reload_geometry "$START_TAB_H" "$START_PADDING" "$STATUS_BAR_H" "status baseline restore"
shot /output/chrome-tabs-00-baseline.png
crop_top /output/chrome-tabs-00-baseline.png /output/chrome-tabs-00-baseline-top.png
BASELINE_H=$(measure_bar_height /output/chrome-tabs-00-baseline.png 0 100 top-baseline)

reload_geometry 60 0 "$STATUS_BAR_H" "tab_height"
HEIGHT_PUBLISH_GROWTH="$PUBLISH_GROWTH"
shot /output/chrome-tabs-01-height-60.png
crop_top /output/chrome-tabs-01-height-60.png /output/chrome-tabs-01-height-60-top.png
HEIGHT_H=$(measure_bar_height /output/chrome-tabs-01-height-60.png 0 100 top-height)
HEIGHT_DELTA=$(image_delta \
    /output/chrome-tabs-00-baseline-top.png /output/chrome-tabs-01-height-60-top.png)

reload_geometry 60 20 "$STATUS_BAR_H" "tab_bar_padding"
PADDING_PUBLISH_GROWTH="$PUBLISH_GROWTH"
shot /output/chrome-tabs-02-padding-20.png
crop_top /output/chrome-tabs-02-padding-20.png /output/chrome-tabs-02-padding-20-top.png
PADDED_H=$(measure_bar_height /output/chrome-tabs-02-padding-20.png 0 100 top-padding)
PADDING_DELTA=$(image_delta \
    /output/chrome-tabs-01-height-60-top.png /output/chrome-tabs-02-padding-20-top.png)

CURRENT_PID=$(pgrep -f '(^|/)scribe-client$' | head -1)
[ "$CURRENT_PID" = "$INITIAL_PID" ] \
    || fail "client restarted during geometry reload ($INITIAL_PID -> $CURRENT_PID)"

# Keep both fail-before signals in one line: pre-fix main paints 34px for every
# capture, so both AE values are zero and neither field reaches the runtime.
if [ "${HEIGHT_DELTA:-0}" -eq 0 ] || [ "${PADDING_DELTA:-0}" -eq 0 ]; then
    fail "tab geometry stayed fixed (tab_height AE=$HEIGHT_DELTA, tab_bar_padding AE=$PADDING_DELTA)"
fi
[ "$BASELINE_H" -eq "$START_BAR_H" ] \
    || fail "baseline top row measured ${BASELINE_H}px, want ${START_BAR_H}px"
[ "$HEIGHT_H" -eq "$HEIGHT_BAR_H" ] \
    || fail "tab_height=60 top row measured ${HEIGHT_H}px"
[ "$PADDED_H" -eq "$PADDED_BAR_H" ] \
    || fail "tab_height=60 + tab_bar_padding=20 measured ${PADDED_H}px"
[ "$HEIGHT_PUBLISH_GROWTH" -gt 0 ] \
    || fail "tab_height edit did not republish pane geometry"
[ "$PADDING_PUBLISH_GROWTH" -gt 0 ] \
    || fail "tab_bar_padding edit did not republish pane geometry"
[ "$INITIAL_W" -eq "$EXPECTED_W" ] \
    || fail "startup width $INITIAL_W != $EXPECTED_W"
[ "$INITIAL_H" -eq "$EXPECTED_H" ] \
    || fail "startup height $INITIAL_H != $EXPECTED_H (configured row + grid + status)"
echo "PHASE 2 PASS: top row hot-reloaded 16 -> 60 -> 80px in pid $INITIAL_PID (AE $HEIGHT_DELTA, $PADDING_DELTA)"

# ── Phase 3: startup baseline leaves the full grid and status bar visible ─────
reload_geometry "$START_TAB_H" "$START_PADDING" "$STATUS_BAR_H" "baseline restore"
shot /output/chrome-bands-00-empty.png
GRID_Y="$START_BAR_H"
GRID_H=$(( WIN_H - START_BAR_H - STATUS_BAR_H ))
[ "$GRID_H" -ge "$GRID_H_MIN" ] \
    || fail "grid viewport $GRID_H px cannot show $ROWS rows (needs $GRID_H_MIN)"

read -r SCREEN_W SCREEN_H <<<"$(xdotool getdisplaygeometry)"
[ "$WIN_W" -le "$SCREEN_W" ] && [ "$WIN_H" -le "$SCREEN_H" ] \
    || fail "window ${WIN_W}x${WIN_H} does not fit the ${SCREEN_W}x${SCREEN_H} screen"
scrot -o /output/chrome-bands-screen.png
FRAME_H=$(convert /output/chrome-bands-screen.png \
    -bordercolor black -fuzz 1% -trim -format "%h" info:)
[ "${FRAME_H:-0}" -ge "$WIN_H" ] \
    || fail "only ${FRAME_H}px of the ${WIN_H}px window is on screen"

BAR_Y=$(( WIN_H - STATUS_BAR_H ))
LAST_ROW_Y=$(( GRID_Y + (ROWS - 1) * ROW_H_X10 / 10 ))
BEFORE_LAST_ROW=$(band_ink /output/chrome-bands-00-empty.png "$LAST_ROW_Y" "$ROW_CROP_H")
scribe-test send "$SESSION" 'clear; seq 1 40; echo GRID_FILL_DONE\n'
scribe-test wait-output "$SESSION" "GRID_FILL_DONE"
sleep 1.0
shot /output/chrome-bands-01-filled.png
LAST_ROW_INK=$(band_ink /output/chrome-bands-01-filled.png "$LAST_ROW_Y" "$ROW_CROP_H")
BAR_INK=$(band_ink /output/chrome-bands-01-filled.png "$BAR_Y" "$STATUS_BAR_H")
[ "${LAST_ROW_INK:-0}" -ge "$INK_MIN" ] \
    || fail "grid row $ROWS is blank after ink $BEFORE_LAST_ROW -> $LAST_ROW_INK"
[ "${BAR_INK:-0}" -ge "$INK_MIN" ] \
    || fail "status bar at y=$BAR_Y is not on screen"
echo "PHASE 3 PASS: ${WIN_W}x${WIN_H} startup shows row $ROWS and the status bar"

# ── Phase 4: prompt chrome takes rows from the pane, not from status ──────────
PROMPT_H=28
PROMPT_Y="$GRID_Y"
crop_band /output/chrome-bands-01-filled.png /output/chrome-bands-prompt-before.png \
    "$PROMPT_Y" "$PROMPT_H"
SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper \
    --provider=claude_code --event=prompt_received --text="$PROMPT_TEXT"
sleep 1.5
shot /output/chrome-bands-02-prompt.png
crop_band /output/chrome-bands-02-prompt.png /output/chrome-bands-prompt-after.png \
    "$PROMPT_Y" "$PROMPT_H"
PROMPT_INK=$(band_ink /output/chrome-bands-02-prompt.png "$PROMPT_Y" "$PROMPT_H")
PROMPT_DELTA=$(image_delta \
    /output/chrome-bands-prompt-before.png /output/chrome-bands-prompt-after.png)
BAR_AFTER=$(band_ink /output/chrome-bands-02-prompt.png "$BAR_Y" "$STATUS_BAR_H")
[ "${PROMPT_INK:-0}" -ge "$INK_MIN" ] || fail "prompt strip never rendered"
[ "${PROMPT_DELTA:-0}" -ge "$PROMPT_DELTA_MIN" ] \
    || fail "prompt band did not repaint ($PROMPT_DELTA changed pixels)"
[ "${BAR_AFTER:-0}" -ge "$INK_MIN" ] \
    || fail "prompt strip pushed the status bar off screen"
echo "PHASE 4 PASS: prompt strip repainted $PROMPT_DELTA pixels without moving status"

# ── Phase 5: lower bar paints and reserves the same configured 80px ───────────
reload_geometry 60 20 "$STATUS_BAR_H" "lower bar geometry"
SPLITS_BEFORE=$(count_log "split the window into a new workspace region")
BARS_BEFORE=$(count_log "lower-region tab bars changed")
focus
xdotool key --clearmodifiers ctrl+alt+minus
wait_for_log_growth "split the window into a new workspace region" "$SPLITS_BEFORE" 15 \
    || fail "ctrl+alt+- never split the workspace"
wait_for_log_growth "lower-region tab bars changed" "$BARS_BEFORE" 20 \
    || fail "stacked workspace never published its lower bar"
sleep 1.5
shot /output/chrome-tabs-03-lower-bar.png
GRID_H=$(( WIN_H - PADDED_BAR_H - STATUS_BAR_H ))
LOWER_TOP=$(( PADDED_BAR_H + (GRID_H + 1) / 2 ))
LOWER_H=$(measure_bar_height /output/chrome-tabs-03-lower-bar.png "$LOWER_TOP" 120 lower)
[ "$LOWER_H" -eq "$PADDED_BAR_H" ] \
    || fail "lower row measured ${LOWER_H}px, want ${PADDED_BAR_H}px"

echo "PHASE 5 PASS: top and lower rows both paint ${PADDED_BAR_H}px"

# ── Phase 6: lower pane paint and hit testing begin below the row ─────────────
# The newly split lower workspace is focused. Fill its erased cells red, enable
# SGR mouse tracking, and block in cat so clicks can be observed in the client
# log. A split pane has a 1px border, so red content starts one pixel after the
# placement rect that begins exactly at the bar's bottom.
focus
xdotool type --delay 1 "printf '\\033[41m\\033[2J\\033[H\\033[?1000h\\033[?1006h'; stty -icanon -echo min 1 time 0; cat -v"
xdotool key --clearmodifiers Return
sleep 1.5
shot /output/chrome-tabs-04-lower-content.png
CONTENT_EXPECTED=$(( LOWER_TOP + PADDED_BAR_H ))
RED_Y=$(first_red_row /output/chrome-tabs-04-lower-content.png \
    $(( WIN_W / 2 )) "$LOWER_TOP" $(( WIN_H - LOWER_TOP - STATUS_BAR_H )))
[ "$RED_Y" -ge "$CONTENT_EXPECTED" ] && [ "$RED_Y" -le $(( CONTENT_EXPECTED + 2 )) ] \
    || fail "lower pane ink began at y=$RED_Y, bar ends at y=$CONTENT_EXPECTED"

MOUSE_BEFORE=$(count_log "mouse input forwarded")
xdotool mousemove --window "$WID" $(( WIN_W / 2 )) $(( LOWER_TOP + PADDED_BAR_H / 2 ))
xdotool click 1
sleep 0.5
MOUSE_IN_BAR=$(count_log "mouse input forwarded")
[ "$MOUSE_IN_BAR" -eq "$MOUSE_BEFORE" ] \
    || fail "lower bar click leaked into the pane hit-test ($MOUSE_BEFORE -> $MOUSE_IN_BAR)"
xdotool mousemove --window "$WID" $(( WIN_W / 2 )) $(( CONTENT_EXPECTED + 12 ))
xdotool click 1
wait_for_log_growth "mouse input forwarded" "$MOUSE_IN_BAR" 10 \
    || fail "click below the lower bar never reached the pane"
echo "PHASE 6 PASS: pane ink starts at y=$RED_Y and hit testing starts below y=$CONTENT_EXPECTED"

# ── Phase 7: compact status controls stay within the configured band ─────────
# The status row is only 8px tall here. Equalize and settings must both accept
# a click at the band's vertical centre rather than extending into the grid.
EQUALIZES_BEFORE=$(count_log "equalized the window layout")
focus
STATUS_CONTROL_Y=$(( WIN_H - STATUS_BAR_H / 2 ))
xdotool mousemove --window "$WID" $(( WIN_W - 30 )) "$STATUS_CONTROL_Y"
xdotool click 1
wait_for_log_growth "equalized the window layout" "$EQUALIZES_BEFORE" 10 \
    || fail "compact status equalize control was not clickable"

SETTINGS_BEFORE=$(count_log "opened the settings window")
xdotool mousemove --window "$WID" $(( WIN_W - 14 )) "$STATUS_CONTROL_Y"
xdotool click 1
wait_for_log_growth "opened the settings window" "$SETTINGS_BEFORE" 10 \
    || fail "compact status settings control was not clickable"
echo "PHASE 7 PASS: compact status equalize and settings controls stayed clickable"

echo ""
echo "PASS: live status/tab geometry, pane reservation, hit testing, and startup sizing agree"
