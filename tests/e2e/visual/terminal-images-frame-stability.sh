#!/bin/bash
# e2e-timeout: 900
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[test#Image Performance and Resource Review#Client Frame Stability Pass]]
#
# The client-side half of the clarification 7C measurement pass.
#
# `terminal-images-performance.sh` meters the server; nothing there can see a
# frame. This script runs the shipped GPUI client against a live pane and
# records what only a window can produce: frame-to-frame stability while an
# image scene is resident, the wall time from a transmission to the first frame
# that paints it, and the view's own projected GPU charge as the cache reports
# it on upload.
#
# It gates on nothing it measures. The only assertions are functional — a
# resident scene must appear in every frame and the renderer must not report a
# failure — because clarification 7C forbids inventing a numeric performance
# threshold and Constitution principle 4 marks the numeric goals inapplicable.
set -euo pipefail

OUT=/output/terminal-images/linux/client
CLIENT_LOG=/output/client-frame-stability.log
WORK=/tmp/image-frame-stability
EVIDENCE="$OUT/frame-stability.tsv"
mkdir -p "$OUT" "$WORK"
: >"$EVIDENCE"

IMG_COLS="${IMG_COLS:-12}"
IMG_ROWS="${IMG_ROWS:-6}"
IMAGE_PX_MIN="${IMAGE_PX_MIN:-4000}"
COLOR_FUZZ="${COLOR_FUZZ:-20}"
RED='#ff0000'
# Frames per idle sample. Six captures over roughly three seconds is enough to
# separate a steady window from one that repaints or flickers on its own.
FRAMES="${FRAMES:-6}"
# Distinct sources uploaded in the multi-image pass; each is its own definition
# and therefore its own projected GPU charge.
UPLOAD_COUNT="${UPLOAD_COUNT:-8}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0
WID=""

fail() {
    echo "FAIL: $1" >&2
    tail -60 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

clean_client_log() { sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"; }

wait_file() {
    local path="$1" timeout_secs="$2" started
    started=$(date +%s)
    until [ -e "$path" ]; do
        [ $(( "$(date +%s)" - started )) -lt "$timeout_secs" ] || return 1
        kill -0 "$CLIENT_PID" 2>/dev/null || return 1
        sleep 0.15
    done
}

wait_client_log() {
    local pattern="$1" timeout_secs="$2" started
    started=$(date +%s)
    until grep -qF "$pattern" "$CLIENT_LOG" 2>/dev/null; do
        [ $(( "$(date +%s)" - started )) -lt "$timeout_secs" ] || return 1
        kill -0 "$CLIENT_PID" 2>/dev/null || return 1
        sleep 0.2
    done
}

now_ms() { echo $(($(date +%s%N) / 1000000)); }

focus() {
    local wid
    wid=$(xdotool search --name '^Scribe$' 2>/dev/null | tail -1)
    [ -n "$wid" ] || fail "no Scribe client window"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
    WID="$wid"
}

# The stable capture: focus, settle, crop the client window out of a full
# screenshot. Used for every frame that is compared against another frame.
capture() {
    focus
    sleep 0.4
    scrot -o "$WORK/fullscreen.png"
    convert "$WORK/fullscreen.png" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

# The fast capture: no focus, no settle. Only used inside the paint-latency
# poll, where the cost of a stable capture would swamp what is being timed.
snap() { import -window "$WID" "$1" 2>/dev/null; }

color_px() {
    local file="$1" color="$2" value
    value=$(convert "$file" -alpha off \
        -fuzz "${COLOR_FUZZ}%" -fill black +opaque "$color" \
        -fill white -opaque "$color" \
        -colorspace Gray -threshold 50% -format '%[fx:mean*w*h]' info:)
    printf '%s' "${value%.*}"
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

record() { printf '%s\t%s\n' "$1" "$2" >>"$EVIDENCE"; }

status_kb() { sed -n "s/^$2:[[:space:]]*\([0-9][0-9]*\) kB/\1/p" "/proc/$1/status"; }

# Highest value the client's upload line reported for a named field.
client_field_max() {
    local field="$1" value
    value=$(clean_client_log | sed -n "s/.* $field=\([0-9][0-9]*\).*/\1/p" | sort -n | tail -1)
    printf '%s' "${value:-0}"
}

uploads_logged() { clean_client_log | grep -cF 'terminal image source uploaded' || true; }

# Capture FRAMES frames back to back and leave the largest pairwise difference
# in FRAME_DIFF_MAX, the smallest in FRAME_DIFF_MIN.
FRAME_DIFF_MAX=0
FRAME_DIFF_MIN=0
sample_frames() {
    local label="$1" index diff first=1
    FRAME_DIFF_MAX=0
    FRAME_DIFF_MIN=0
    for index in $(seq 1 "$FRAMES"); do
        capture "$WORK/$label-$index.png"
    done
    for index in $(seq 2 "$FRAMES"); do
        diff=$(pixel_diff "$WORK/$label-$((index - 1)).png" "$WORK/$label-$index.png")
        [ "$diff" -gt "$FRAME_DIFF_MAX" ] && FRAME_DIFF_MAX=$diff
        if [ "$first" = 1 ]; then
            FRAME_DIFF_MIN=$diff
            first=0
        elif [ "$diff" -lt "$FRAME_DIFF_MIN" ]; then
            FRAME_DIFF_MIN=$diff
        fi
    done
    cp "$WORK/$label-$FRAMES.png" "$OUT/$label.png"
}

# ---------------------------------------------------------------------------
# Payloads. One solid red source sized in cells, plus UPLOAD_COUNT distinct
# sources for the projected-GPU pass. Every transfer stays inside the frozen
# 4096-byte single-chunk ceiling and suppresses its reply with `q=2`, so the
# pane's own shell never reads a protocol result as input.
# ---------------------------------------------------------------------------
python3 - "$WORK" "$IMG_COLS" "$IMG_ROWS" "$UPLOAD_COUNT" <<'PY'
import base64
import pathlib
import sys

work = pathlib.Path(sys.argv[1])
cols, rows, uploads = sys.argv[2], sys.argv[3], int(sys.argv[4])
RIS = b"\x1bc"


def kitty(payload: bytes, **keys: object) -> bytes:
    control = ",".join(f"{name}={value}" for name, value in keys.items())
    return b"\x1b_G" + control.encode() + b";" + base64.b64encode(payload) + b"\x1b\\"


(work / "text.bin").write_bytes(RIS + b"frame stability text baseline\r\n" * 8)
(work / "image.bin").write_bytes(
    RIS + kitty(b"\xff\x00\x00" * 32 * 32, a="T", f=24, s=32, v=32,
                c=cols, r=rows, i=21, q=2)
)
# Distinct sources: one differing byte per image is enough to make each a
# separate definition, and each is placed so the view actually uploads it.
# One file per image, so each arrives in its own committed read.
for index in range(uploads):
    pixels = bytes([index, 0x20, 0xC0]) * 24 * 24
    (work / f"upload-{index}.bin").write_bytes(
        kitty(pixels, a="T", f=24, s=24, v=24, c=2, r=1, i=40 + index, q=2)
        + b"\r\n"
    )
# The same count again, this time as one uninterrupted burst, so the pass can
# record how many of a single committed read's definitions reach the view.
burst = [RIS]
for index in range(uploads):
    pixels = bytes([0x40 + index, 0xC0, 0x20]) * 24 * 24
    burst.append(kitty(pixels, a="T", f=24, s=24, v=24, c=2, r=1,
                       i=60 + index, q=2))
    burst.append(b"\r\n")
(work / "burst.bin").write_bytes(b"".join(burst))
(work / "clear.bin").write_bytes(RIS)
# Scroll the pane so the resident placement is repainted against a moving grid.
(work / "scroll.bin").write_bytes(b"\x1b[999;1H\n\n\n")
PY

: >"$WORK/ready.cmd"

# One file-driven loop in the pane: exactly one command line is ever typed, so
# nothing the terminal writes back can reach a prompt.
cat >"$WORK/driver.sh" <<EOF
stty -echo 2>/dev/null || true
while :; do
    if [ -e "$WORK/step" ]; then
        name=\$(cat "$WORK/step")
        rm -f "$WORK/step"
        [ -e "$WORK/\$name.bin" ] && cat "$WORK/\$name.bin"
        [ -e "$WORK/\$name.cmd" ] && . "$WORK/\$name.cmd"
        touch "$WORK/done-\$name"
    fi
    sleep 0.1
done
EOF

start_step() {
    rm -f "$WORK/done-$1"
    printf '%s' "$1" >"$WORK/step.tmp"
    mv "$WORK/step.tmp" "$WORK/step"
}

run_step() {
    start_step "$1"
    wait_file "$WORK/done-$1" 60 || fail "$1 never completed in the pane"
    sleep 1.0
}

# ---------------------------------------------------------------------------
# Phase 0: an image-capable client on a live pane. Capability is what latches a
# session, so the entrypoint's ordinary client is replaced with one that opts
# in to the renderer subset.
# ---------------------------------------------------------------------------
kill "${SCRIBE_CLIENT_PID:?visual entrypoint did not export SCRIBE_CLIENT_PID}" 2>/dev/null || true
wait "$SCRIBE_CLIENT_PID" 2>/dev/null || true
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0

: >"$CLIENT_LOG"
SCRIBE_TERMINAL_IMAGES=1 LIBGL_ALWAYS_SOFTWARE=1 \
    RUST_LOG="${RUST_LOG:-scribe_client=info}" \
    scribe-client >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!
trap 'kill "$CLIENT_PID" 2>/dev/null || true' EXIT
wait_client_log "pane adopted a session" 60 \
    || fail "the image-capable client never adopted a session"
sleep 2.0
focus
xdotool type --delay 8 --clearmodifiers "bash $WORK/driver.sh"
xdotool key --clearmodifiers Return
run_step ready
CLIENT_RSS_START=$(status_kb "$CLIENT_PID" VmRSS)
echo "PHASE 0 PASS: an image-capable client is driving a live pane"

# ---------------------------------------------------------------------------
# Phase 1: the text-only frame baseline. Whatever the window does on its own
# while idle — a blinking cursor, a status tick — belongs to this number, and
# the image phases are read against it rather than against zero.
# ---------------------------------------------------------------------------
run_step text
sample_frames text-idle
TEXT_DIFF_MAX=$FRAME_DIFF_MAX
TEXT_DIFF_MIN=$FRAME_DIFF_MIN
record text_idle_frame_diff_max "$TEXT_DIFF_MAX"
record text_idle_frame_diff_min "$TEXT_DIFF_MIN"
echo "MEASURED text-only idle frames: pairwise diff ${TEXT_DIFF_MIN}..${TEXT_DIFF_MAX} px"

# ---------------------------------------------------------------------------
# Phase 2: transmission to first painted frame. The poll deliberately skips the
# focus-and-settle capture; a stable capture costs about as long as the whole
# interval being measured.
# ---------------------------------------------------------------------------
UPLOADS_BEFORE=$(uploads_logged)
START=$(now_ms)
start_step image
PAINT_MS=0
DEADLINE=$((SECONDS + 60))
while :; do
    snap "$WORK/poll.png" || true
    if [ -s "$WORK/poll.png" ] && [ "$(color_px "$WORK/poll.png" "$RED")" -ge "$IMAGE_PX_MIN" ]; then
        PAINT_MS=$(($(now_ms) - START))
        break
    fi
    [ "$SECONDS" -lt "$DEADLINE" ] || fail "the transmitted image never reached a frame"
done
wait_file "$WORK/done-image" 30 || fail "the image step never completed in the pane"
record transmit_to_paint_ms "$PAINT_MS"
echo "MEASURED transmit-to-paint: ${PAINT_MS}ms"

[ "$(uploads_logged)" -gt "$UPLOADS_BEFORE" ] \
    || fail "the view painted an image without recording an upload"
record first_upload_projected_gpu_bytes "$(client_field_max projected_gpu_bytes)"

# ---------------------------------------------------------------------------
# Phase 3: frame stability with the scene resident. The assertion is presence,
# not steadiness: a placement that survives in canonical state but drops out of
# a frame is a defect no measurement threshold is needed to name.
# ---------------------------------------------------------------------------
sample_frames image-idle
IMAGE_DIFF_MAX=$FRAME_DIFF_MAX
IMAGE_DIFF_MIN=$FRAME_DIFF_MIN
IMAGE_PX_MIN_SEEN=""
IMAGE_PX_MAX_SEEN=0
for index in $(seq 1 "$FRAMES"); do
    count=$(color_px "$WORK/image-idle-$index.png" "$RED")
    [ "$count" -ge "$IMAGE_PX_MIN" ] \
        || fail "frame $index of the resident scene painted only $count px of red"
    [ -z "$IMAGE_PX_MIN_SEEN" ] && IMAGE_PX_MIN_SEEN=$count
    [ "$count" -lt "$IMAGE_PX_MIN_SEEN" ] && IMAGE_PX_MIN_SEEN=$count
    [ "$count" -gt "$IMAGE_PX_MAX_SEEN" ] && IMAGE_PX_MAX_SEEN=$count
done
record image_idle_frame_diff_max "$IMAGE_DIFF_MAX"
record image_idle_frame_diff_min "$IMAGE_DIFF_MIN"
record image_px_min "$IMAGE_PX_MIN_SEEN"
record image_px_max "$IMAGE_PX_MAX_SEEN"
echo "PHASE 3 PASS: every idle frame kept the scene (${IMAGE_PX_MIN_SEEN}..${IMAGE_PX_MAX_SEEN} px), pairwise diff ${IMAGE_DIFF_MIN}..${IMAGE_DIFF_MAX} px"

# ---------------------------------------------------------------------------
# Phase 4: repaint under motion. Scrolling moves the placement against the grid
# and forces a full repaint from the same uploaded source.
# ---------------------------------------------------------------------------
START=$(now_ms)
run_step scroll
capture "$OUT/scrolled.png"
SCROLL_MS=$(($(now_ms) - START))
SCROLLED_PX=$(color_px "$OUT/scrolled.png" "$RED")
record scroll_repaint_ms "$SCROLL_MS"
record scrolled_px "$SCROLLED_PX"
echo "MEASURED scroll repaint: ${SCROLL_MS}ms leaving ${SCROLLED_PX} px on screen"

# ---------------------------------------------------------------------------
# Phase 5: projected GPU accounting across several distinct sources. The number
# recorded is the cache's own charge at its high-water mark, not a re-derivation
# from the definitions.
# ---------------------------------------------------------------------------
UPLOADS_BEFORE=$(uploads_logged)
# A source is uploaded by the paint that first needs it, so each image is given
# its own step and its own frame rather than a guessed settle time.
for index in $(seq 0 $((UPLOAD_COUNT - 1))); do
    run_step "upload-$index"
    capture "$OUT/uploads.png"
done
UPLOADS_AFTER=$(uploads_logged)
[ "$UPLOADS_AFTER" -ge $((UPLOADS_BEFORE + UPLOAD_COUNT)) ] \
    || fail "only $((UPLOADS_AFTER - UPLOADS_BEFORE)) of $UPLOAD_COUNT sources reached the view"
PROJECTED_GPU_BYTES=$(client_field_max projected_gpu_bytes)
SOURCES_MAX=$(client_field_max sources)
# The frozen per-view ceiling. This one is a security limit, not a performance
# goal, so it is asserted rather than merely recorded.
[ "$PROJECTED_GPU_BYTES" -le 268435456 ] \
    || fail "the view charged $PROJECTED_GPU_BYTES projected GPU bytes, past the frozen ceiling"
CLIENT_RSS_END=$(status_kb "$CLIENT_PID" VmRSS)
CLIENT_RSS_PEAK=$(status_kb "$CLIENT_PID" VmHWM)
record uploads_recorded "$((UPLOADS_AFTER - UPLOADS_BEFORE))"
record projected_gpu_bytes_peak "$PROJECTED_GPU_BYTES"
record cached_sources_peak "$SOURCES_MAX"
record client_rss_kb_start "$CLIENT_RSS_START"
record client_rss_kb_end "$CLIENT_RSS_END"
record client_rss_kb_peak "$CLIENT_RSS_PEAK"
echo "MEASURED view resources: ${PROJECTED_GPU_BYTES}B projected across ${SOURCES_MAX} sources, client RSS ${CLIENT_RSS_START}kB -> ${CLIENT_RSS_END}kB"

# ---------------------------------------------------------------------------
# Phase 6: the same count of distinct sources delivered as one uninterrupted
# burst. A shell that prints several images in one write commits them in one
# read, and every definition a read commits is published, so anything short of
# the full count is a silently lossy screen rather than a slow one.
# ---------------------------------------------------------------------------
run_step clear
BURST_UPLOADS_BEFORE=$(uploads_logged)
run_step burst
DEADLINE=$((SECONDS + 20))
while [ "$(uploads_logged)" -lt $((BURST_UPLOADS_BEFORE + UPLOAD_COUNT)) ]; do
    [ "$SECONDS" -lt "$DEADLINE" ] || break
    capture "$OUT/burst.png"
done
capture "$OUT/burst.png"
BURST_UPLOADED=$(( $(uploads_logged) - BURST_UPLOADS_BEFORE ))
record single_burst_sources_transmitted "$UPLOAD_COUNT"
record single_burst_sources_uploaded "$BURST_UPLOADED"
[ "$BURST_UPLOADED" -ge "$UPLOAD_COUNT" ] \
    || fail "only $BURST_UPLOADED of $UPLOAD_COUNT sources of one committed read reached the view"
echo "MEASURED single-burst delivery: ${BURST_UPLOADED} of ${UPLOAD_COUNT} sources reached the view"

# The renderer must not have degraded while all of this was measured.
clean_client_log | grep -qF 'terminal image placement paint failed' \
    && fail "the renderer reported a failed placement paint"
clean_client_log | grep -qF 'terminal image source preparation failed' \
    && fail "the renderer reported a failed source preparation"
kill -0 "$CLIENT_PID" 2>/dev/null || fail "the client exited during the measurement pass"

python3 - "$OUT/frame-stability.json" "$EVIDENCE" "$FRAMES" "$UPLOAD_COUNT" <<'PY'
import json
import sys

path, evidence_path, frames, uploads = sys.argv[1:]
measurements = {}
with open(evidence_path, encoding="utf-8") as source:
    for line in source:
        name, value = line.rstrip().split("\t")
        measurements[name] = int(value)

with open(path, "w", encoding="utf-8") as handle:
    json.dump(
        {
            "schema_version": 1,
            "engine": "scribe terminal image frame stability and view resources",
            "surface": "linux_docker_running_client",
            "status": "measured",
            "numeric_performance_thresholds": "inapplicable",
            "threshold_rationale": (
                "clarification 7C; Constitution principle 4 permits marking numeric "
                "performance goals inapplicable while the named-command measurement "
                "requirement still applies"
            ),
            "workload": {
                "frames_per_sample": int(frames),
                "distinct_uploaded_sources": int(uploads),
            },
            "assertions": {
                "scene_present_in_every_idle_frame": True,
                "renderer_reported_no_failure": True,
                "client_survived": True,
            },
            "measurements": measurements,
        },
        handle,
        indent=2,
        sort_keys=True,
    )
    handle.write("\n")
PY

echo "PASS: terminal image frame stability and view resource measurements recorded in $OUT/frame-stability.json"
