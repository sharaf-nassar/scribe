#!/bin/bash
# e2e-timeout: 600
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[test#Layered GPUI Terminal Images#Renderer phases and lifecycle stay pixel-stable]]
set -euo pipefail

OUT=/output/terminal-images/linux/renderer
LOG="$OUT/renderer.log"
CLEAN_LOG="$OUT/renderer-clean.log"
MEASUREMENTS="$OUT/measurements.tsv"
mkdir -p "$OUT"
: >"$MEASUREMENTS"

fail() {
    echo "FAIL: $1" >&2
    tail -100 "$CLEAN_LOG" 2>/dev/null >&2 || tail -100 "$LOG" 2>/dev/null >&2 || true
    exit 1
}

wait_for_log() {
    local pattern="$1" timeout_secs="${2:-30}" started
    started=$(date +%s)
    while ! grep -qF "$pattern" "$LOG" 2>/dev/null; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        kill -0 "$PROBE_PID" 2>/dev/null || return 1
        sleep 0.2
    done
}

find_window() {
    xdotool search --name 'Scribe Terminal Image Renderer' 2>/dev/null | tail -1
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no terminal image renderer window"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.4
    printf '%s' "$wid"
}

capture() {
    local wid
    wid=$(focus)
    import -window "$wid" "$1"
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

log_value() {
    local name="$1"
    sed -n "s/.*${name}[=: ]\+\([0-9.]\+\).*/\1/p" "$CLEAN_LOG" | head -1
}

event_value() {
    local event="$1" name="$2"
    grep -F "$event" "$CLEAN_LOG" \
        | sed -n "s/.*${name}[=: ]\+\([-0-9.]\+\).*/\1/p" \
        | head -1
}

cell_coordinate() {
    local origin="$1" metric="$2" cell="$3" fraction="$4"
    awk -v origin="$origin" -v metric="$metric" -v cell="$cell" \
        -v fraction="$fraction" -v scale="$SCALE_FACTOR" \
        'BEGIN { printf "%.0f", (origin + metric * (cell + fraction)) * scale }'
}

assert_cell_rgb() {
    local image="$1" col="$2" row="$3" x_fraction="$4" y_fraction="$5"
    local expected_r="$6" expected_g="$7" expected_b="$8" tolerance="$9" label="${10}"
    local x y actual actual_r actual_g actual_b
    x=$(cell_coordinate "$GRID_LEFT" "$CELL_WIDTH" "$col" "$x_fraction")
    y=$(cell_coordinate "$GRID_TOP" "$LINE_HEIGHT" "$row" "$y_fraction")
    actual=$(convert "$image" -format \
        "%[fx:int(255*p{$x,$y}.r+0.5)],%[fx:int(255*p{$x,$y}.g+0.5)],%[fx:int(255*p{$x,$y}.b+0.5)]" info:)
    IFS=, read -r actual_r actual_g actual_b <<<"$actual"
    awk -v ar="$actual_r" -v ag="$actual_g" -v ab="$actual_b" \
        -v er="$expected_r" -v eg="$expected_g" -v eb="$expected_b" \
        -v tolerance="$tolerance" \
        'BEGIN {
            if ((ar-er < 0 ? er-ar : ar-er) > tolerance ||
                (ag-eg < 0 ? eg-ag : ag-eg) > tolerance ||
                (ab-eb < 0 ? eb-ab : ab-eb) > tolerance) exit 1
        }' || fail "$label at $x,$y was rgb($actual), expected rgb($expected_r,$expected_g,$expected_b) ±$tolerance"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$label" "$x" "$y" "$actual_r" "$actual_g" "$actual_b" \
        "$expected_r" "$expected_g" "$expected_b" "$tolerance" >>"$MEASUREMENTS"
}

record_scalar() {
    printf 'scalar:%s\t%s\t0\t0\t0\t0\t0\t0\t0\t0\n' "$1" "$2" >>"$MEASUREMENTS"
}

# Replace only the container-local client started by the visual entrypoint.
kill "${SCRIBE_CLIENT_PID:?visual entrypoint did not export SCRIBE_CLIENT_PID}" 2>/dev/null || true
wait "$SCRIBE_CLIENT_PID" 2>/dev/null || true
: >"$LOG"
GPUI_X11_SCALE_FACTOR=2 \
RUST_LOG="${RUST_LOG:-scribe_client=info},gpui_wgpu=info" \
    scribe-client --terminal-image-renderer-probe >"$LOG" 2>&1 &
PROBE_PID=$!
trap 'kill "$PROBE_PID" 2>/dev/null || true' EXIT

wait_for_log 'terminal image renderer ready' 45 || fail "renderer did not reach first paint"
sleep 1
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
grep -Eq 'render_images_created[=: ]+10' "$CLEAN_LOG" \
    || fail "renderer did not create one source per fixture definition"

SCALE_FACTOR=$(log_value scale_factor)
GRID_LEFT=$(log_value grid_left)
GRID_TOP=$(log_value grid_top)
CELL_WIDTH=$(log_value cell_width)
LINE_HEIGHT=$(log_value line_height)
[ -n "$SCALE_FACTOR" ] && [ -n "$GRID_LEFT" ] && [ -n "$GRID_TOP" ] \
    && [ -n "$CELL_WIDTH" ] && [ -n "$LINE_HEIGHT" ] \
    || fail "renderer did not log measured grid geometry"
awk -v scale="$SCALE_FACTOR" 'BEGIN { exit !(scale > 1.99 && scale < 2.01) }' \
    || fail "GPUI_X11_SCALE_FACTOR=2 produced scale factor $SCALE_FACTOR"

capture "$OUT/00-layered.png"
IMAGE_WIDTH=$(identify -format '%w' "$OUT/00-layered.png")
IMAGE_HEIGHT=$(identify -format '%h' "$OUT/00-layered.png")
[ "$IMAGE_WIDTH" -ge 1600 ] && [ "$IMAGE_HEIGHT" -ge 950 ] \
    || fail "2x GPUI window was only ${IMAGE_WIDTH}x${IMAGE_HEIGHT} device pixels"
record_scalar dpi_scale_factor "$SCALE_FACTOR"
record_scalar initial_device_width "$IMAGE_WIDTH"
record_scalar initial_device_height "$IMAGE_HEIGHT"

# Crop, whole-placement scaling, and source alpha use independent regions.
assert_cell_rgb "$OUT/00-layered.png" 1 1 .25 .2 5 5 6 4 crop_offset_top_edge
assert_cell_rgb "$OUT/00-layered.png" 1 1 .25 .6 0 255 0 3 crop_top_green
assert_cell_rgb "$OUT/00-layered.png" 1 4 .25 .75 255 255 0 3 crop_bottom_yellow
assert_cell_rgb "$OUT/00-layered.png" 0 2 .75 .5 5 5 6 4 crop_left_edge
assert_cell_rgb "$OUT/00-layered.png" 5 2 .25 .5 5 5 6 4 crop_right_edge
assert_cell_rgb "$OUT/00-layered.png" 2 5 .5 .2 255 255 0 3 crop_offset_bottom_fraction
assert_cell_rgb "$OUT/00-layered.png" 2 5 .5 .6 5 5 6 4 crop_bottom_edge
assert_cell_rgb "$OUT/00-layered.png" 9 2 .25 .5 0 190 230 3 scale_inside
assert_cell_rgb "$OUT/00-layered.png" 8 2 .75 .5 5 5 6 4 scale_left_edge
assert_cell_rgb "$OUT/00-layered.png" 15 2 .25 .5 5 5 6 4 scale_right_edge
assert_cell_rgb "$OUT/00-layered.png" 11 5 .5 .25 5 5 6 4 scale_bottom_edge
assert_cell_rgb "$OUT/00-layered.png" 19 2 .5 .5 130 3 88 6 alpha_composite

# Six paint phases: every assertion is a distinct cell/pixel relationship.
assert_cell_rgb "$OUT/00-layered.png" 3 9 .25 .25 220 40 40 3 phase1_deep_default_bg
assert_cell_rgb "$OUT/00-layered.png" 3 8 .25 .25 45 65 110 3 phase2_background_occludes
assert_cell_rgb "$OUT/00-layered.png" 15 8 .25 .25 40 210 80 3 phase3_negative_above_bg
assert_cell_rgb "$OUT/00-layered.png" 16 8 .5 .5 228 228 231 8 phase4_box_above_negative
assert_cell_rgb "$OUT/00-layered.png" 41 9 .25 .25 235 190 25 3 phase1_sixel_default_bg
assert_cell_rgb "$OUT/00-layered.png" 42 9 .5 .5 228 228 231 8 phase4_box_above_sixel
assert_cell_rgb "$OUT/00-layered.png" 28 8 .5 .5 30 90 230 3 phase5_positive_covers_glyph
assert_cell_rgb "$OUT/00-layered.png" 29 8 .2 .2 228 228 231 4 phase6_cursor_above_positive
assert_cell_rgb "$OUT/00-layered.png" 31 9 .5 .5 63 63 70 4 phase6_selection_above_positive
assert_cell_rgb "$OUT/00-layered.png" 32 9 .5 .5 59 130 246 4 phase6_find_over_selection
assert_cell_rgb "$OUT/00-layered.png" 43 9 .25 .25 180 30 210 3 sixel_later_completion_wins
assert_cell_rgb "$OUT/00-layered.png" 43 10 .25 .25 180 30 210 3 sixel_before_scroll

# Placeholder evidence includes transparent cell backing, 1:1 aspect fit,
# 32-bit identity, 8-bit inheritance, and deterministic missing placement ID.
assert_cell_rgb "$OUT/00-layered.png" 2 16 .5 .25 85 30 105 4 placeholder_aspect_top_band
assert_cell_rgb "$OUT/00-layered.png" 2 16 .5 .75 85 30 105 4 placeholder_transparent_backing
assert_cell_rgb "$OUT/00-layered.png" 3 16 .5 .75 0 255 0 3 placeholder_32bit_green
assert_cell_rgb "$OUT/00-layered.png" 2 17 .5 .15 0 0 255 3 placeholder_32bit_blue
assert_cell_rgb "$OUT/00-layered.png" 3 17 .5 .15 255 255 0 3 placeholder_32bit_yellow
assert_cell_rgb "$OUT/00-layered.png" 2 17 .5 .75 85 30 105 4 placeholder_aspect_bottom_band
assert_cell_rgb "$OUT/00-layered.png" 2 20 .5 .25 255 0 0 3 placeholder_default_placement
assert_cell_rgb "$OUT/00-layered.png" 3 20 .5 .25 255 0 0 3 placeholder_8bit_inheritance
assert_cell_rgb "$OUT/00-layered.png" 3 20 .5 .75 0 0 255 3 placeholder_8bit_aspect

# An additional live source exceeds the configured view ceiling. The complete frame
# must stay identical: no earlier atlas tile can be dropped or reused.
focus >/dev/null
xdotool key --clearmodifiers q
wait_for_log 'terminal image renderer pressure rejected without eviction' 15 \
    || fail "cache pressure rejection was not observed"
sleep 1
capture "$OUT/01-pressure.png"
PRESSURE_DIFF=$(pixel_diff "$OUT/00-layered.png" "$OUT/01-pressure.png")
[ "$PRESSURE_DIFF" -eq 0 ] || fail "pressure changed $PRESSURE_DIFF queued pixels"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
grep -Eq 'terminal image renderer pressure rejected without eviction.*render_images_created[=: ]+10' "$CLEAN_LOG" \
    || fail "pressure created or replaced a baseline RenderImage"
grep -Eq 'terminal image renderer pressure rejected without eviction.*atlas_drops[=: ]+0' "$CLEAN_LOG" \
    || fail "pressure dropped a live atlas tile"
grep -Eq 'terminal image renderer pressure rejected without eviction.*projected_gpu_bytes[=: ]+91136' "$CLEAN_LOG" \
    || fail "pressure exceeded or shrank the hard cache bound"
PRESSURE_REJECTIONS=$(event_value 'terminal image renderer pressure rejected without eviction' pressure_rejections)
PRESSURE_CREATED=$(event_value 'terminal image renderer pressure rejected without eviction' render_images_created)
PRESSURE_DROPS=$(event_value 'terminal image renderer pressure rejected without eviction' atlas_drops)
PRESSURE_BYTES=$(event_value 'terminal image renderer pressure rejected without eviction' projected_gpu_bytes)
record_scalar pressure_pixel_diff "$PRESSURE_DIFF"
record_scalar pressure_rejections "$PRESSURE_REJECTIONS"
record_scalar pressure_render_images_created "$PRESSURE_CREATED"
record_scalar pressure_atlas_drops "$PRESSURE_DROPS"
record_scalar pressure_projected_gpu_bytes "$PRESSURE_BYTES"

# Atlas recovery and explicit between-frame eviction recreate identical pixels.
focus >/dev/null
xdotool key --clearmodifiers d
wait_for_log 'terminal image renderer device-loss atlas invalidated' 15 \
    || fail "device-loss atlas invalidation proxy was not observed"
sleep 1
capture "$OUT/02-device-loss-proxy.png"
DEVICE_DIFF=$(pixel_diff "$OUT/01-pressure.png" "$OUT/02-device-loss-proxy.png")
[ "$DEVICE_DIFF" -eq 0 ] || fail "atlas recovery changed $DEVICE_DIFF pixels"
record_scalar device_loss_pixel_diff "$DEVICE_DIFF"

focus >/dev/null
xdotool key --clearmodifiers e
wait_for_log 'terminal image renderer cache evicted' 15 \
    || fail "explicit renderer eviction was not observed"
sleep 1
capture "$OUT/03-eviction-recreated.png"
EVICTION_DIFF=$(pixel_diff "$OUT/01-pressure.png" "$OUT/03-eviction-recreated.png")
[ "$EVICTION_DIFF" -eq 0 ] || fail "eviction recreation changed $EVICTION_DIFF pixels"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
grep -Eq 'terminal image renderer cache evicted.*final_reference_drops[=: ]+10' "$CLEAN_LOG" \
    || fail "explicit eviction did not drop ten final references"
record_scalar eviction_pixel_diff "$EVICTION_DIFF"

# First production scroll stores the current margin without changing source
# mapping, while Sixel completion order and partial clipping stay observable.
focus >/dev/null
xdotool key --clearmodifiers s
wait_for_log 'terminal image renderer first scroll mapping' 15 \
    || fail "first production scroll/margin stage was not observed"
sleep 1
capture "$OUT/04-first-scroll.png"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'anchor_row=0' 'source_y=0' 'source_height=64' \
    'destination_rows=4' 'pixel_offset_y=8' 'clip_top=1' 'clip_bottom=5'; do
    grep -F 'terminal image renderer first scroll mapping' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "first scroll mapping missing $expected"
done
for field in anchor_row source_y source_height destination_rows pixel_offset_y clip_top clip_bottom; do
    value=$(event_value 'terminal image renderer first scroll mapping' "$field")
    record_scalar "first_scroll_$field" "$value"
done
assert_cell_rgb "$OUT/04-first-scroll.png" 2 0 .5 .75 5 5 6 4 first_scroll_margin_top
assert_cell_rgb "$OUT/04-first-scroll.png" 2 1 .5 .5 0 255 0 3 first_scroll_original_source_top
assert_cell_rgb "$OUT/04-first-scroll.png" 2 3 .5 .5 255 255 0 3 first_scroll_original_source_bottom
# The half-open margin ends at row 5, so the placement's last destination row
# stays visible and its pixel-offset spill is clipped mid-cell. A clip interval
# that loses its exclusive bottom row would blank this fraction entirely.
assert_cell_rgb "$OUT/04-first-scroll.png" 2 4 .5 .2 255 255 0 3 first_scroll_margin_bottom_fraction
assert_cell_rgb "$OUT/04-first-scroll.png" 2 4 .5 .7 5 5 6 4 first_scroll_margin_bottom_edge
assert_cell_rgb "$OUT/04-first-scroll.png" 56 2 .5 .2 0 190 230 3 first_scroll_offset_fraction
assert_cell_rgb "$OUT/04-first-scroll.png" 56 2 .5 .7 5 5 6 4 first_scroll_offset_edge
assert_cell_rgb "$OUT/04-first-scroll.png" 41 9 .25 .25 235 190 25 3 first_scroll_sixel_earlier_visible
assert_cell_rgb "$OUT/04-first-scroll.png" 43 9 .25 .25 180 30 210 3 first_scroll_sixel_later_wins
assert_cell_rgb "$OUT/04-first-scroll.png" 42 9 .5 .5 228 228 231 8 first_scroll_sixel_under_text
assert_cell_rgb "$OUT/04-first-scroll.png" 43 10 .25 .25 5 5 6 4 first_scroll_sixel_bottom_edge
FIRST_SCROLL_DIFF=$(pixel_diff "$OUT/03-eviction-recreated.png" "$OUT/04-first-scroll.png")
[ "$FIRST_SCROLL_DIFF" -gt 0 ] || fail "first production scroll changed no pixels"
record_scalar first_scroll_pixel_diff "$FIRST_SCROLL_DIFF"

# Repeated scroll shifts the stored logical clip while the original source,
# destination extent, and Y offset remain unchanged.
focus >/dev/null
xdotool key --clearmodifiers r
wait_for_log 'terminal image renderer repeated scroll mapping' 15 \
    || fail "repeated production scroll stage was not observed"
sleep 1
capture "$OUT/05-repeated-scroll.png"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'anchor_row=-1' 'source_y=0' 'source_height=64' \
    'destination_rows=4' 'pixel_offset_y=8' 'clip_top=1' 'clip_bottom=4'; do
    grep -F 'terminal image renderer repeated scroll mapping' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "repeated scroll mapping missing $expected"
done
for field in anchor_row source_y source_height destination_rows pixel_offset_y clip_top clip_bottom; do
    value=$(event_value 'terminal image renderer repeated scroll mapping' "$field")
    record_scalar "repeated_scroll_$field" "$value"
done
assert_cell_rgb "$OUT/05-repeated-scroll.png" 2 1 .5 .2 0 255 0 3 repeated_scroll_source_before_split
assert_cell_rgb "$OUT/05-repeated-scroll.png" 2 1 .5 .7 255 255 0 3 repeated_scroll_source_after_split
assert_cell_rgb "$OUT/05-repeated-scroll.png" 2 3 .5 .2 255 255 0 3 repeated_scroll_bottom_fraction
assert_cell_rgb "$OUT/05-repeated-scroll.png" 2 3 .5 .7 5 5 6 4 repeated_scroll_destination_edge
assert_cell_rgb "$OUT/05-repeated-scroll.png" 56 1 .5 .2 0 190 230 3 repeated_scroll_offset_fraction
assert_cell_rgb "$OUT/05-repeated-scroll.png" 56 1 .5 .7 5 5 6 4 repeated_scroll_offset_edge
REPEATED_SCROLL_DIFF=$(pixel_diff "$OUT/04-first-scroll.png" "$OUT/05-repeated-scroll.png")
[ "$REPEATED_SCROLL_DIFF" -gt 0 ] || fail "repeated production scroll changed no pixels"
record_scalar repeated_scroll_pixel_diff "$REPEATED_SCROLL_DIFF"

# Production deletion evidence uses effective clipped extents for physical
# images and protects virtual placeholder placements from non-image scopes.
focus >/dev/null
xdotool key --clearmodifiers x
wait_for_log 'terminal image renderer deletion evidence' 15 \
    || fail "production deletion evidence was not observed"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'row_outside_placements=2' 'row_inside_placements=1' \
    'column_outside_placements=2' 'column_inside_placements=1' \
    'cell_inside_placements=1' 'virtual_placement_placements=2' \
    'z_index_placements=1'; do
    grep -F 'terminal image renderer deletion evidence' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "deletion evidence missing $expected"
    field=${expected%%=*}
    value=$(event_value 'terminal image renderer deletion evidence' "$field")
    record_scalar "delete_$field" "$value"
done
wait_for_log 'terminal image renderer hard deletion evidence' 15 \
    || fail "production hard deletion evidence was not observed"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'cell_unplaced_definition=1' 'cell_hard_definitions=2' \
    'row_unplaced_definition=1' 'row_hard_definitions=2' \
    'column_unplaced_definition=1' 'column_hard_definitions=2' \
    'z_index_unplaced_definition=1' 'z_index_hard_definitions=2' \
    'placement_unplaced_definition=1' 'placement_hard_definitions=2' \
    'all_hard_placements=1' \
    'all_hard_definitions=2' 'virtual_image_hard_placements=1' \
    'virtual_image_hard_definitions=2' 'unplaced_image_hard_placements=2' \
    'unplaced_image_hard_definitions=2' 'unplaced_image_hard_target_present=0'; do
    grep -F 'terminal image renderer hard deletion evidence' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "hard deletion evidence missing $expected"
    field=${expected%%=*}
    value=$(event_value 'terminal image renderer hard deletion evidence' "$field")
    record_scalar "delete_$field" "$value"
done

# A placement outside the scroll margin first receives a resize envelope. An
# unrelated margin scroll must preserve both its raw mapping and visible pixels.
focus >/dev/null
xdotool key --clearmodifiers o
wait_for_log 'terminal image renderer off-margin resized mapping' 15 \
    || fail "off-margin resize stage was not observed"
sleep 1
capture "$OUT/06-off-margin-resized.png"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'anchor_row=12' 'source_y=0' 'pixel_offset_x=5' \
    'pixel_offset_y=8' 'clip_top=12' 'clip_left=50' \
    'clip_bottom=15' 'clip_right=55'; do
    grep -F 'terminal image renderer off-margin resized mapping' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "off-margin resize mapping missing $expected"
done
for field in anchor_row source_y pixel_offset_x pixel_offset_y clip_top clip_left clip_bottom clip_right; do
    value=$(event_value 'terminal image renderer off-margin resized mapping' "$field")
    record_scalar "off_margin_resized_$field" "$value"
done
assert_cell_rgb "$OUT/06-off-margin-resized.png" 51 12 .5 .7 0 190 230 3 off_margin_resized_top
assert_cell_rgb "$OUT/06-off-margin-resized.png" 51 14 .5 .2 0 190 230 3 off_margin_resized_bottom_fraction
assert_cell_rgb "$OUT/06-off-margin-resized.png" 51 14 .5 .7 5 5 6 4 off_margin_resized_bottom_edge

focus >/dev/null
xdotool key --clearmodifiers u
wait_for_log 'terminal image renderer off-margin scrolled mapping' 15 \
    || fail "off-margin unrelated-scroll stage was not observed"
sleep 1
capture "$OUT/07-off-margin-scrolled.png"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'anchor_row=12' 'source_y=0' 'pixel_offset_x=5' \
    'pixel_offset_y=8' 'clip_top=12' 'clip_left=50' \
    'clip_bottom=15' 'clip_right=55'; do
    grep -F 'terminal image renderer off-margin scrolled mapping' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "off-margin scroll mapping missing $expected"
done
for field in anchor_row source_y pixel_offset_x pixel_offset_y clip_top clip_left clip_bottom clip_right; do
    value=$(event_value 'terminal image renderer off-margin scrolled mapping' "$field")
    record_scalar "off_margin_scrolled_$field" "$value"
done
assert_cell_rgb "$OUT/07-off-margin-scrolled.png" 51 12 .5 .7 0 190 230 3 off_margin_scrolled_top
assert_cell_rgb "$OUT/07-off-margin-scrolled.png" 51 14 .5 .2 0 190 230 3 off_margin_scrolled_bottom_fraction
assert_cell_rgb "$OUT/07-off-margin-scrolled.png" 51 14 .5 .7 5 5 6 4 off_margin_scrolled_bottom_edge
OFF_MARGIN_DIFF=$(pixel_diff "$OUT/06-off-margin-resized.png" "$OUT/07-off-margin-scrolled.png")
[ "$OFF_MARGIN_DIFF" -gt 0 ] || fail "unrelated margin scroll changed no related pixels"
record_scalar off_margin_frame_pixel_diff "$OFF_MARGIN_DIFF"

# Resize intersects the stored logical mask. Original source/destination and
# pixel offsets still drive mapping at the current 2x cell metrics.
focus >/dev/null
xdotool key --clearmodifiers z
wait_for_log 'terminal image renderer resize clip mapping' 15 \
    || fail "production resize clip stage was not observed"
sleep 1
capture "$OUT/08-resize-clip.png"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
for expected in 'source_y=0' 'source_height=64' 'destination_rows=4' \
    'pixel_offset_y=8' 'clip_top=1' 'clip_bottom=2' 'clip_right=5'; do
    grep -F 'terminal image renderer resize clip mapping' "$CLEAN_LOG" \
        | grep -q "$expected" || fail "resize clip mapping missing $expected"
done
for field in source_y source_height destination_rows pixel_offset_y clip_top clip_bottom clip_right; do
    value=$(event_value 'terminal image renderer resize clip mapping' "$field")
    record_scalar "resize_clip_$field" "$value"
done
assert_cell_rgb "$OUT/08-resize-clip.png" 2 1 .5 .2 0 255 0 3 resize_source_before_split
assert_cell_rgb "$OUT/08-resize-clip.png" 2 1 .5 .7 255 255 0 3 resize_source_after_split
assert_cell_rgb "$OUT/08-resize-clip.png" 2 2 .5 .2 5 5 6 4 resize_row_clip_edge
assert_cell_rgb "$OUT/08-resize-clip.png" 56 1 .5 .2 0 190 230 3 resize_offset_inside
assert_cell_rgb "$OUT/08-resize-clip.png" 57 1 .5 .2 5 5 6 4 resize_column_clip_edge
RESIZE_DIFF=$(pixel_diff "$OUT/07-off-margin-scrolled.png" "$OUT/08-resize-clip.png")
[ "$RESIZE_DIFF" -gt 0 ] || fail "production resize clip changed no pixels"
record_scalar resize_clip_pixel_diff "$RESIZE_DIFF"

# Closing the pane drops every surviving session source before empty paint.
focus >/dev/null
xdotool key --clearmodifiers p
wait_for_log 'terminal image renderer pane cache dropped' 15 \
    || fail "pane-close cache cleanup was not observed"
sleep 1
capture "$OUT/09-pane-close.png"
CLOSE_DIFF=$(pixel_diff "$OUT/08-resize-clip.png" "$OUT/09-pane-close.png")
[ "$CLOSE_DIFF" -gt 0 ] || fail "pane close did not remove rendered images"
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
grep -Eq 'terminal image renderer pane cache dropped.*final_reference_drops[=: ]+20' "$CLEAN_LOG" \
    || fail "pane close did not drop every recreated cache source"
record_scalar pane_close_pixel_diff "$CLOSE_DIFF"

python3 - "$OUT/renderer.json" "$MEASUREMENTS" <<'PY'
import json
import sys

path, measurements_path = sys.argv[1:]
pixels = {}
scalars = {}
with open(measurements_path, encoding="utf-8") as source:
    for line in source:
        label, x, y, ar, ag, ab, er, eg, eb, tolerance = line.rstrip().split("\t")
        if label.startswith("scalar:"):
            value = float(x)
            scalars[label.removeprefix("scalar:")] = int(value) if value.is_integer() else value
            continue
        pixels[label] = {
            "coordinate": [int(x), int(y)],
            "observed_rgb": [int(ar), int(ag), int(ab)],
            "expected_rgb": [int(er), int(eg), int(eb)],
            "tolerance": int(tolerance),
        }

evidence = {
    "schema": 3,
    "platform": "linux",
    "renderer_boundary": "production-committed-image-scene",
    "dpi": {
        "scale_factor": scalars["dpi_scale_factor"],
        "device_size": [scalars["initial_device_width"], scalars["initial_device_height"]],
    },
    "pixel_assertions": pixels,
    "cache_pressure": {
        "frame_pixel_diff": scalars["pressure_pixel_diff"],
        "pressure_rejections": scalars["pressure_rejections"],
        "render_images_created": scalars["pressure_render_images_created"],
        "atlas_drops": scalars["pressure_atlas_drops"],
        "projected_gpu_bytes": scalars["pressure_projected_gpu_bytes"],
    },
    "device_loss_proxy_pixel_diff": scalars["device_loss_pixel_diff"],
    "eviction_recreation_pixel_diff": scalars["eviction_pixel_diff"],
    "first_scroll_pixel_diff": scalars["first_scroll_pixel_diff"],
    "first_scroll_mapping": {
        "anchor_row": scalars["first_scroll_anchor_row"],
        "source_y": scalars["first_scroll_source_y"],
        "source_height": scalars["first_scroll_source_height"],
        "destination_rows": scalars["first_scroll_destination_rows"],
        "pixel_offset_y": scalars["first_scroll_pixel_offset_y"],
        "clip_top": scalars["first_scroll_clip_top"],
        "clip_bottom": scalars["first_scroll_clip_bottom"],
    },
    "repeated_scroll_pixel_diff": scalars["repeated_scroll_pixel_diff"],
    "off_margin_frame_pixel_diff": scalars["off_margin_frame_pixel_diff"],
    "off_margin_mapping": {
        "resized": {
            key.removeprefix("off_margin_resized_"): value
            for key, value in scalars.items()
            if key.startswith("off_margin_resized_")
        },
        "after_unrelated_scroll": {
            key.removeprefix("off_margin_scrolled_"): value
            for key, value in scalars.items()
            if key.startswith("off_margin_scrolled_")
        },
    },
    "deletion": {
        key.removeprefix("delete_"): value
        for key, value in scalars.items()
        if key.startswith("delete_")
    },
    "resize_clip_pixel_diff": scalars["resize_clip_pixel_diff"],
    "resize_clip_mapping": {
        "source_y": scalars["resize_clip_source_y"],
        "source_height": scalars["resize_clip_source_height"],
        "destination_rows": scalars["resize_clip_destination_rows"],
        "pixel_offset_y": scalars["resize_clip_pixel_offset_y"],
        "clip_top": scalars["resize_clip_clip_top"],
        "clip_bottom": scalars["resize_clip_clip_bottom"],
        "clip_right": scalars["resize_clip_clip_right"],
    },
    "repeated_scroll_mapping": {
        "anchor_row": scalars["repeated_scroll_anchor_row"],
        "source_y": scalars["repeated_scroll_source_y"],
        "source_height": scalars["repeated_scroll_source_height"],
        "destination_rows": scalars["repeated_scroll_destination_rows"],
        "pixel_offset_y": scalars["repeated_scroll_pixel_offset_y"],
        "clip_top": scalars["repeated_scroll_clip_top"],
        "clip_bottom": scalars["repeated_scroll_clip_bottom"],
    },
    "pane_close_pixel_diff": scalars["pane_close_pixel_diff"],
    "captures": {
        "initial": "00-layered.png",
        "pressure": "01-pressure.png",
        "device_loss_proxy": "02-device-loss-proxy.png",
        "eviction_recreated": "03-eviction-recreated.png",
        "first_scroll": "04-first-scroll.png",
        "repeated_scroll": "05-repeated-scroll.png",
        "off_margin_resized": "06-off-margin-resized.png",
        "off_margin_scrolled": "07-off-margin-scrolled.png",
        "resize_clip": "08-resize-clip.png",
        "pane_close": "09-pane-close.png",
    },
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(evidence, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "PASS: layered GPUI terminal image renderer"
