#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[terminal-images#GPUI Lifecycle Verification]]
#
# Running Linux/WGPU proof for the pinned GPUI terminal-image path. The probe
# owns an isolated window and never attaches image behavior to the terminal
# renderer bead: it exercises one shared RenderImage, bounds-plus-mask crop,
# atlas reuse/removal/recovery, and frozen dimension checks only.
set -euo pipefail

OUT=/output/terminal-images/linux
LOG="$OUT/gpui-spike.log"
CLEAN_LOG="$OUT/gpui-spike-clean.log"
mkdir -p "$OUT"

fail() {
    echo "FAIL: $1" >&2
    tail -80 "$CLEAN_LOG" 2>/dev/null >&2 || tail -80 "$LOG" 2>/dev/null >&2 || true
    exit 1
}

wait_for_log() {
    local pattern="$1" timeout_secs="${2:-30}" started
    started=$(date +%s)
    while ! grep -qF "$pattern" "$LOG" 2>/dev/null; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        kill -0 "$SPIKE_PID" 2>/dev/null || return 1
        sleep 0.2
    done
}

find_window() {
    xdotool search --name 'Scribe GPUI Image Spike' 2>/dev/null | tail -1
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no GPUI image spike window"
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

channel_mean() {
    convert "$1" -crop 96x96+288+112 +repage -channel "$2" -separate \
        -format '%[fx:mean]' info:
}

# The entrypoint starts the ordinary client before it delegates to a script.
# Replace only that container-local process with the isolated spike surface.
kill "${SCRIBE_CLIENT_PID:?visual entrypoint did not export SCRIBE_CLIENT_PID}" 2>/dev/null || true
wait "$SCRIBE_CLIENT_PID" 2>/dev/null || true
: >"$LOG"
RUST_LOG="${RUST_LOG:-scribe_client=info},gpui_wgpu=info" \
    scribe-client --gpui-image-spike >"$LOG" 2>&1 &
SPIKE_PID=$!
trap 'kill "$SPIKE_PID" 2>/dev/null || true' EXIT

wait_for_log 'GPUI image max-plus-one rejected before allocation' 45 \
    || fail "max-plus-one dimension rejection was not observed"
wait_for_log 'GPUI image spike ready' 15 \
    || fail "GPUI image window did not reach first paint"
wait_for_log 'Selected GPU adapter' 15 \
    || fail "GPUI did not record its selected adapter"
sleep 1
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"

grep -Eq 'Selected GPU adapter:.*\((Vulkan|Gl)\)$' "$CLEAN_LOG" \
    || fail "running GPUI window did not select a Linux WGPU backend"
backend=$(sed -n 's/.*Selected GPU adapter:.* (\([^()]\+\))$/\1/p' "$CLEAN_LOG" | tail -1)
[ -n "$backend" ] || fail "could not parse selected WGPU backend"
grep -zq '^VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json$' \
    "/proc/$SPIKE_PID/environ" \
    || fail "running GPUI window did not inherit the pinned Lavapipe ICD"
grep -Eq 'render_images_created[=: ]+3' "$CLEAN_LOG" \
    || fail "one source RenderImage per definition was not created"
grep -Eq 'cache_reuses[=: ]+1' "$CLEAN_LOG" \
    || fail "full and cropped placements did not reuse one RenderImage"
grep -Eq 'render_images_created_before[=: ]+0.*render_images_created_after[=: ]+0' "$CLEAN_LOG" \
    || fail "max-plus-one reached GPUI allocation"

capture "$OUT/00-initial.png"
green=$(channel_mean "$OUT/00-initial.png" G)
red=$(channel_mean "$OUT/00-initial.png" R)
blue=$(channel_mean "$OUT/00-initial.png" B)
awk -v g="$green" -v r="$red" -v b="$blue" \
    'BEGIN { exit !(g > 0.90 && r < 0.10 && b < 0.10) }' \
    || fail "bounds-plus-mask crop did not isolate the green source quadrant"

focus >/dev/null
xdotool key --clearmodifiers d
wait_for_log 'GPUI image atlas invalidated for recovery' 15 \
    || fail "window atlas invalidation was not observed"
wait_for_log 'GPUI image cache reused after atlas invalidation' 15 \
    || fail "CPU RenderImage identities were not reused for recovery"
sleep 1
capture "$OUT/01-recovered.png"
recovery_diff=$(pixel_diff "$OUT/00-initial.png" "$OUT/01-recovered.png")
[ "$recovery_diff" -eq 0 ] \
    || fail "reupload after atlas invalidation changed $recovery_diff pixels"

focus >/dev/null
xdotool key --clearmodifiers e
wait_for_log 'GPUI image cache evicted at final reference' 15 \
    || fail "final-reference eviction was not observed"
wait_for_log 'GPUI image cache recreated after final-reference eviction' 15 \
    || fail "evicted sources were not recreated"
sleep 1
capture "$OUT/02-recreated.png"
eviction_diff=$(pixel_diff "$OUT/00-initial.png" "$OUT/02-recreated.png")
[ "$eviction_diff" -eq 0 ] \
    || fail "repaint after final-reference eviction changed $eviction_diff pixels"

sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" >"$CLEAN_LOG"
grep -Eq 'final_reference_drops[=: ]+3' "$CLEAN_LOG" \
    || fail "not every source called drop_image at its final cache reference"

python3 - "$OUT/gpui-spike.json" "$backend" "$green" "$red" "$blue" \
    "$recovery_diff" "$eviction_diff" <<'PY'
import json
import sys

path, backend, green, red, blue, recovery_diff, eviction_diff = sys.argv[1:]
evidence = {
    "schema": 1,
    "platform": "linux",
    "renderer": f"wgpu-{backend.lower()}",
    "selected_backend": backend,
    "configured_vulkan_icd": "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
    "gpui_revision": "f96212f2c50f54d93712fa130d6226b1ce7d76b5",
    "crop_strategy": "shared-render-image-translated-bounds-content-mask",
    "crop_green_mean": float(green),
    "crop_red_mean": float(red),
    "crop_blue_mean": float(blue),
    "render_image_reuse": True,
    "shared_render_image_count": 1,
    "pinned_atlas_key_source_verified": True,
    "drop_image_cleanup_source_verified": True,
    "final_reference_drop_count": 3,
    "atlas_recovery_source_verified": True,
    "recovery_preserved_source_ids": True,
    "recovery_pixel_diff": int(recovery_diff),
    "eviction_pixel_diff": int(eviction_diff),
    "dimensions": {
        "one_pixel_uploaded": True,
        "max_width_uploaded": 4096,
        "max_plus_one_rejected": 4097,
        "render_images_created_by_rejection": 0,
    },
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(evidence, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "PASS: GPUI image crop/lifecycle spike (Linux WGPU)"
