#!/bin/bash
# e2e-timeout: 900
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[test#Running Client Terminal Images#A running client paints every image path]]
#
# Visual E2E: the RUNNING GPUI client, fed by a real server over the real IPC
# seam, paints what a terminal application transmitted.
#
# Every other terminal-image gate stops one seam short of this one:
# `terminal-image-renderer.sh` drives the renderer from a synthetic scene,
# `terminal-images-functional.sh` drives the server with no window at all, and
# `terminal-image-apps.sh` proves the pinned applications reach the server's
# counters. Only this script asserts pixels in the shipped client's window for
# bytes a shell wrote on a PTY, so it is the one gate that fails when any link
# between the PTY reader and the painted frame is missing.
#
# The window is driven through XTEST (`xdotool type` / `xdotool key`, never
# `--window`): GPUI reads input through XInput2 and ignores the synthetic
# XSendEvent input window-targeted xdotool delivers.
set -euo pipefail

OUT=/output/terminal-images/linux/client
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CLIENT_LOG=/output/client-images-visual.log
WORK=/tmp/image-visual
EVIDENCE="$OUT/phases.tsv"
mkdir -p "$OUT" "$WORK"
: >"$EVIDENCE"

# A solid image box is IMG_COLS x IMG_ROWS cells. At the container's default
# font that is 99x112 px, so a painted box clears 10000 matching pixels while
# an unpainted frame holds none at all.
IMG_COLS="${IMG_COLS:-12}"
IMG_ROWS="${IMG_ROWS:-6}"
IMAGE_PX_MIN="${IMAGE_PX_MIN:-4000}"
# The pinned Unicode-placeholder fixture is one row of two cells backed by a
# single source pixel, so its 1:1 aspect fit is a ~17x17 px box.
PLACEHOLDER_PX_MIN="${PLACEHOLDER_PX_MIN:-40}"
# What "no image" means. Every colour below is fully saturated and appears
# nowhere in Scribe's own chrome, so an image-free frame measures exactly zero.
ABSENT_PX_MAX="${ABSENT_PX_MAX:-30}"
COLOR_FUZZ="${COLOR_FUZZ:-20}"

RED='#ff0000'
CYAN='#00ffff'
MAGENTA='#ff00ff'
YELLOW='#ffff00'
ORANGE='#ff7f00'
GREEN='#00ff00'
APP_RED='#d02020'

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0
ACTIVE_WID=

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server image log tail ---" >&2
    sed 's/\x1b\[[0-9;]*m//g' "$SERVER_LOG" 2>/dev/null | grep -F 'terminal image' | tail -40 >&2 || true
    exit 1
}

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

focus() {
    local wid
    wid="${ACTIVE_WID:-}"
    [ -n "$wid" ] || wid=$(xdotool search --name '^Scribe$' 2>/dev/null | tail -1)
    [ -n "$wid" ] || fail "no Scribe client window"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
    printf '%s' "$wid"
}

# Match a restored session to the GPUI window created for it. Each backend logs
# its attach before the X11 id for that same window.
window_for_session() {
    sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG" | awk -v wanted="$1" '
        /attaching to session session_id=/ {
            for (i = 1; i <= NF; i++)
                if ($i ~ /^session_id=/) session = substr($i, 12)
        }
        /X11 active-window guard enabled window=/ && session == wanted {
            for (i = 1; i <= NF; i++)
                if ($i ~ /^window=/) { print substr($i, 8); exit }
        }
    '
}

wait_session_window() {
    local session="$1" timeout_secs="$2" started wid
    started=$(date +%s)
    while :; do
        wid=$(window_for_session "$session")
        [ -n "$wid" ] && { printf '%s' "$wid"; return 0; }
        [ $(( "$(date +%s)" - started )) -lt "$timeout_secs" ] || return 1
        kill -0 "$CLIENT_PID" 2>/dev/null || return 1
        sleep 0.2
    done
}

# Capture the client window alone; a full-screen scrot also catches openbox
# chrome, whose pixels belong to no phase here.
capture() {
    focus >/dev/null
    sleep 0.5
    scrot -o "$WORK/fullscreen.png"
    convert "$WORK/fullscreen.png" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

# Pixels in the window whose colour is within COLOR_FUZZ of $2.
#
# Two `-opaque` passes rather than a per-pixel text dump: ImageMagick does the
# whole frame in C, and the same reduction as a `txt:` scan through sed/awk
# costs seconds per capture across the twenty captures below.
color_px() {
    local file="$1" color="$2" value
    value=$(convert "$file" -alpha off \
        -fuzz "${COLOR_FUZZ}%" -fill black +opaque "$color" \
        -fill white -opaque "$color" \
        -colorspace Gray -threshold 50% -format '%[fx:mean*w*h]' info:)
    printf '%s' "${value%.*}"
}

assert_present() {
    local file="$1" color="$2" floor="$3" label="$4" count
    count=$(color_px "$file" "$color")
    [ "$count" -ge "$floor" ] \
        || fail "$label: the client painted $count px of $color, wanted at least $floor"
    printf '%s\t%s\t%s\tpresent\n' "$label" "$color" "$count" >>"$EVIDENCE"
    printf '%s' "$count"
}

assert_absent() {
    local file="$1" color="$2" label="$3" count
    count=$(color_px "$file" "$color")
    [ "$count" -le "$ABSENT_PX_MAX" ] \
        || fail "$label: the client still painted $count px of $color, wanted at most $ABSENT_PX_MAX"
    printf '%s\t%s\t%s\tabsent\n' "$label" "$color" "$count" >>"$EVIDENCE"
    printf '%s' "$count"
}

# ---------------------------------------------------------------------------
# Payloads.
#
# Every solid source is 32x32 (24x24 for the four-channel one) and carries
# `c=`/`r=`, so the painted box is a known number of CELLS whatever the
# container's font metrics turn out to be, while the base64 of each transfer
# stays inside the frozen 4096-byte single-chunk ceiling. A one-pixel source
# would upscale through its own edge falloff and never reach a solid colour.
# ---------------------------------------------------------------------------
python3 - "$WORK" "$IMG_COLS" "$IMG_ROWS" <<'PY'
import base64
import pathlib
import subprocess
import sys

work = pathlib.Path(sys.argv[1])
cols, rows = sys.argv[2], sys.argv[3]

RIS = b"\x1bc"


def kitty(payload: bytes, **keys: object) -> bytes:
    control = ",".join(f"{name}={value}" for name, value in keys.items())
    return b"\x1b_G" + control.encode() + b";" + base64.b64encode(payload) + b"\x1b\\"


def write(name: str, *parts: bytes) -> None:
    (work / name).write_bytes(b"".join(parts))


# f=24 RGB, a solid red source scaled across the cell box. `q=2` keeps the
# protocol reply off the PTY the pane's own shell is reading.
red_rgb = kitty(b"\xff\x00\x00" * 32 * 32,
                a="T", f=24, s=32, v=32, c=cols, r=rows, i=11, q=2)
write("rgb.bin", RIS, red_rgb)

# f=32 RGBA: an opaque cyan left half beside a fully transparent right half.
# The painted box is half cyan and half whatever the terminal already had
# there, which is what separates a respected alpha channel from an ignored one.
rgba_rows = [b"\x00\xff\xff\xff" * 12 + b"\x00\xff\xff\x00" * 12 for _ in range(24)]
write("rgba.bin", RIS, kitty(b"".join(rgba_rows),
                             a="T", f=32, s=24, v=24, c=cols, r=rows, i=12, q=2))

# f=100 PNG, encoded by the container's ImageMagick so the bytes are a real
# compressed PNG and not a hand-rolled approximation of one.
png = subprocess.run(
    ["convert", "-size", "32x32", "xc:#ff00ff", "png:-"],
    capture_output=True, check=True,
).stdout
write("png.bin", RIS, kitty(png, a="T", f=100, c=cols, r=rows, i=13, q=2))

# Sixel: a solid yellow raster, sized in pixels the way Sixel always is.
bands = [b"#0;2;100;100;0" + b"!240~" for _ in range(20)]
write("sixel.bin", RIS, b'\x1bP0;1;0q"1;1;240;120' + b"-".join(bands) + b"\x1b\\")

# The pinned Unicode-placeholder fixture, byte for byte: a virtual placement
# plus its placeholder cells. No released application drives Kitty virtual
# placements through an unrecognised terminal, so the owned corpus is the only
# truthful source for this path.
placeholder = bytes.fromhex(
    pathlib.Path("/tests/fixtures/terminal-images/kitty-unicode-placeholder.hex")
    .read_text().strip()
)
write("placeholder.bin", RIS, placeholder)

# Z-order: the same green text under and over the same red box. `z` is the only
# byte that differs between the two payloads.
text = b"\x1b[38;2;0;255;0m" + b"M" * 12 + b"\x1b[0m"
for name, z in (("z-below.bin", -1), ("z-above.bin", 1)):
    write(
        name,
        RIS,
        kitty(b"\xff\x00\x00" * 32 * 32,
              a="T", f=24, s=32, v=32, c=cols, r=rows, i=14, z=z, q=2),
        b"\x1b[H",
        text,
    )

# Crop: a source whose top half is red and bottom half orange, displayed with
# the source rectangle narrowed to the orange half. The box is the same size as
# every other, so "the crop was applied" is the absence of red, not a
# difference in area. The rectangle starts two rows below the tone boundary
# because a bilinear sample on the boundary row itself legitimately mixes the
# neighbouring texel, which is a filtering edge and not a crop.
two_tone = b"\xff\x00\x00" * 32 * 16 + b"\xff\x7f\x00" * 32 * 16
write(
    "crop.bin",
    RIS,
    kitty(two_tone, a="T", f=24, s=32, v=32, c=cols, r=rows, i=15, y=18, h=12, q=2),
)

# Delete: drop image 11 and everything placed from it.
write("delete.bin", b"\x1b_Ga=d,d=I,i=11,q=2\x1b\\")

# Reset: RIS on its own.
write("reset.bin", RIS)

# Scroll: park the cursor on the last row and push three lines through it, so
# the box that started at the home row is clipped by the top margin.
write("scroll.bin", b"\x1b[999;1H\n\n\n")

# A transmit declaring 4097 pixels of width — one past the frozen
# max_width_pixels — with a payload far too small to be that image.
write("rejected.bin", RIS, b"\x1b_Ga=T,f=24,s=4097,v=1,t=d;AAAA\x1b\\")
PY

convert -size 64x64 xc:"$APP_RED" "$WORK/app.png"
cat >"$WORK/chafa-kitty.cmd" <<EOF
printf '\033c'
chafa --format kitty --probe off --size ${IMG_COLS}x${IMG_ROWS} $WORK/app.png
EOF
cat >"$WORK/chafa-sixel.cmd" <<EOF
printf '\033c'
chafa --format sixels --probe off --size ${IMG_COLS}x${IMG_ROWS} $WORK/app.png
EOF
: >"$WORK/ready.cmd"

# ---------------------------------------------------------------------------
# The pane driver.
#
# Typing every step at the shell prompt cannot work here: a rejected transfer
# makes the server write a protocol error back to the PTY, readline takes those
# bytes as input, and the NEXT typed command is corrupted by them. The pane
# instead runs one loop that takes its instructions from files, so exactly one
# line is ever typed and nothing the terminal writes back can reach a command
# line. `stty -echo` keeps any such reply off the screen as well.
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# Phase 0: an image-capable client on a live pane.
#
# Capability is what latches a session: until a viewer announces the renderer
# subset the server leaves the session text-only. Terminal images are on by
# default, so the entrypoint's client is replaced only to own the log and the
# relaunch cycle, not to opt anything in.
# ---------------------------------------------------------------------------
kill "${SCRIBE_CLIENT_PID:?visual entrypoint did not export SCRIBE_CLIENT_PID}" 2>/dev/null || true
wait "$SCRIBE_CLIENT_PID" 2>/dev/null || true
# Release the daemon's window the way cold-restart.sh does.
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0

launch_client() {
    local wanted="${1:-}"
    : >"$CLIENT_LOG"
    ACTIVE_WID=
    LIBGL_ALWAYS_SOFTWARE=1 \
        scribe-client >"$CLIENT_LOG" 2>&1 &
    CLIENT_PID=$!
    wait_client_log "attaching to session" 45 \
        || fail "the image-capable client never attached to a session"
    if [ -n "$wanted" ]; then
        ADOPTED_SESSION="$wanted"
    else
        ADOPTED_SESSION=$(sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG" \
            | sed -n 's/.*attaching to session session_id=\([^ ]*\).*/\1/p' | head -1)
    fi
    ACTIVE_WID=$(wait_session_window "$ADOPTED_SESSION" 45) \
        || fail "the client never opened a window for session $ADOPTED_SESSION"
    sleep 0.5
    focus >/dev/null
}

# Start one client process. It reopens every server-retained window, so a
# requested session is selected by its logged X11 id rather than by relaunching
# until the server happens to assign that window first.
start_client() {
    launch_client "${1:-}"
}

start_client
trap 'kill "$CLIENT_PID" 2>/dev/null || true' EXIT

# Run one step in the pane and capture the window once the driver reports done.
# The optional second argument names the capture, so a step replayed as another
# phase's precondition keeps its own evidence file instead of overwriting one.
run_step() {
    local name="$1" label="${2:-$1}"
    rm -f "$WORK/done-$name"
    printf '%s' "$name" >"$WORK/step.tmp"
    mv "$WORK/step.tmp" "$WORK/step"
    wait_file "$WORK/done-$name" 60 || fail "$name never completed in the pane"
    sleep 1.2
    capture "$OUT/$label.png"
}

# Prove the pane is live and typed input lands before any pixel assertion, so a
# broken rig cannot be mistaken for an image that failed to paint.
IMAGE_SESSION="$ADOPTED_SESSION"
focus >/dev/null
xdotool type --delay 8 --clearmodifiers "bash $WORK/driver.sh"
xdotool key --clearmodifiers Return
run_step ready
echo "PHASE 0 PASS: an image-capable client attached to $IMAGE_SESSION and typing reaches its pane"

# ---------------------------------------------------------------------------
# Phase 1-5: one painted box per transmission format, plus the placeholder.
# ---------------------------------------------------------------------------
run_step rgb
RGB_PX=$(assert_present "$OUT/rgb.png" "$RED" "$IMAGE_PX_MIN" rgb_classic)
echo "PHASE 1 PASS: an f=24 RGB transmit painted $RGB_PX px of red"

run_step rgba
RGBA_PX=$(assert_present "$OUT/rgba.png" "$CYAN" "$IMAGE_PX_MIN" rgba_alpha)
[ "$RGBA_PX" -le $(( RGB_PX * 3 / 4 )) ] \
    || fail "rgba_alpha: $RGBA_PX px of cyan is not a half-transparent box beside $RGB_PX px of opaque red"
echo "PHASE 2 PASS: an f=32 RGBA transmit painted $RGBA_PX px of cyan and left its transparent half alone"

run_step png
PNG_PX=$(assert_present "$OUT/png.png" "$MAGENTA" "$IMAGE_PX_MIN" png_classic)
echo "PHASE 3 PASS: an f=100 PNG transmit painted $PNG_PX px of magenta"

run_step sixel
SIXEL_PX=$(assert_present "$OUT/sixel.png" "$YELLOW" "$IMAGE_PX_MIN" sixel_raster)
echo "PHASE 4 PASS: a Sixel raster painted $SIXEL_PX px of yellow"

run_step placeholder
PLACEHOLDER_PX=$(assert_present "$OUT/placeholder.png" "$RED" "$PLACEHOLDER_PX_MIN" unicode_placeholder)
echo "PHASE 5 PASS: the pinned Unicode-placeholder fixture painted $PLACEHOLDER_PX px of red"

# ---------------------------------------------------------------------------
# Phase 6: z-order. The same text and the same box; only `z` differs.
# ---------------------------------------------------------------------------
run_step z-below
assert_present "$OUT/z-below.png" "$RED" "$IMAGE_PX_MIN" zorder_below_image >/dev/null
Z_TEXT=$(assert_present "$OUT/z-below.png" "$GREEN" 40 zorder_below_text)
run_step z-above
assert_present "$OUT/z-above.png" "$RED" "$IMAGE_PX_MIN" zorder_above_image >/dev/null
assert_absent "$OUT/z-above.png" "$GREEN" zorder_above_text >/dev/null
echo "PHASE 6 PASS: z=-1 left $Z_TEXT px of text above the box and z=1 covered it"

# ---------------------------------------------------------------------------
# Phase 7: crop. The narrowed source rectangle is the only reason red is gone.
# ---------------------------------------------------------------------------
run_step crop
CROP_PX=$(assert_present "$OUT/crop.png" "$ORANGE" "$IMAGE_PX_MIN" crop_kept_row)
assert_absent "$OUT/crop.png" "$RED" crop_dropped_row >/dev/null
echo "PHASE 7 PASS: a y=18,h=12 source rectangle painted $CROP_PX px of orange and no red"

# ---------------------------------------------------------------------------
# Phase 8: scroll. The box starts on the home row, so three scrolled lines clip
# it against the top margin instead of moving it out of the pane.
# ---------------------------------------------------------------------------
run_step rgb scroll-before
BEFORE_SCROLL=$(color_px "$OUT/scroll-before.png" "$RED")
run_step scroll
AFTER_SCROLL=$(assert_present "$OUT/scroll.png" "$RED" 1000 scroll_clipped)
[ "$AFTER_SCROLL" -lt "$(( BEFORE_SCROLL * 4 / 5 ))" ] \
    || fail "scroll_clipped: $AFTER_SCROLL px of red after scrolling is not less than $BEFORE_SCROLL px before it"
echo "PHASE 8 PASS: scrolling clipped the box from $BEFORE_SCROLL px to $AFTER_SCROLL px of red"

# ---------------------------------------------------------------------------
# Phase 9: resize. A smaller window re-lays the grid out under a live image.
# ---------------------------------------------------------------------------
run_step rgb resize-before
BEFORE_RESIZE=$(color_px "$OUT/resize-before.png" "$RED")
WID=$(focus)
ORIGINAL_W="$WIN_W"
ORIGINAL_H="$WIN_H"
xdotool windowsize --sync "$WID" 800 600
sleep 2.0
capture "$OUT/resize.png"
RESIZE_PX=$(assert_present "$OUT/resize.png" "$RED" 1000 resize_survived)
echo "PHASE 9 PASS: resizing ${ORIGINAL_W}x${ORIGINAL_H} -> ${WIN_W}x${WIN_H} kept $RESIZE_PX px of red (was $BEFORE_RESIZE)"

# ---------------------------------------------------------------------------
# Phase 10: split. The image pane keeps its box while sharing the window.
# ---------------------------------------------------------------------------
xdotool key --clearmodifiers ctrl+shift+backslash
sleep 2.5
capture "$OUT/split.png"
SPLIT_PX=$(assert_present "$OUT/split.png" "$RED" 500 split_survived)
# Exactly one close: the split left the NEW pane focused, and a second chord
# would take the image pane down with it.
xdotool key --clearmodifiers ctrl+shift+w
sleep 2.5
capture "$OUT/split-closed.png"
echo "PHASE 10 PASS: a vertical split kept $SPLIT_PX px of red in the image pane"

# The remaining phases drive the pane again, so give the window back the
# geometry the rest of the corpus measured.
WID=$(focus)
xdotool windowsize --sync "$WID" "$ORIGINAL_W" "$ORIGINAL_H"
sleep 1.5
run_step ready split-restored
assert_present "$OUT/split-restored.png" "$RED" 1000 pane_restored >/dev/null

# ---------------------------------------------------------------------------
# Phase 11-12: delete and reset both take the box off the screen.
# ---------------------------------------------------------------------------
run_step delete
assert_absent "$OUT/delete.png" "$RED" delete_cleared >/dev/null
echo "PHASE 11 PASS: a=d,d=I deleted the image from the painted scene"

run_step rgb reset-before
assert_present "$OUT/reset-before.png" "$RED" "$IMAGE_PX_MIN" reset_precondition >/dev/null
run_step reset
assert_absent "$OUT/reset.png" "$RED" reset_cleared >/dev/null
echo "PHASE 12 PASS: RIS cleared the painted scene"

# ---------------------------------------------------------------------------
# Phase 13: replay. The relaunched viewer never saw the transmission, so the
# whole scene has to come back out of canonical server state.
#
# Nothing is typed into the pane between the relaunch and the capture: the
# attach itself pays the new sink's replay debt, so an idle application is not
# a reason for a viewer to sit in front of an imageless pane.
# ---------------------------------------------------------------------------
run_step rgb replay-before
assert_present "$OUT/replay-before.png" "$RED" "$IMAGE_PX_MIN" replay_precondition >/dev/null
kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
start_client "$IMAGE_SESSION"
sleep 1.5
capture "$OUT/replay.png"
REPLAY_PX=$(assert_present "$OUT/replay.png" "$RED" "$IMAGE_PX_MIN" replay_restored)
echo "PHASE 13 PASS: a relaunched client replayed $REPLAY_PX px of red out of canonical state"

# ---------------------------------------------------------------------------
# Phase 14: the pinned applications, in pixels.
#
# `terminal-image-apps.sh` owns the frozen versions and the server-side counters
# for the whole corpus; what it cannot assert is that the frames those counters
# describe were painted. One Kitty and one Sixel producer close that.
# ---------------------------------------------------------------------------
chafa --version | grep -qF 'Chafa version 1.18.2' || fail "chafa is not the pinned 1.18.2"
run_step chafa-kitty
APP_KITTY_PX=$(assert_present "$OUT/chafa-kitty.png" "$APP_RED" 1000 app_chafa_kitty)
run_step chafa-sixel
APP_SIXEL_PX=$(assert_present "$OUT/chafa-sixel.png" "$APP_RED" 1000 app_chafa_sixel)
echo "PHASE 14 PASS: pinned chafa 1.18.2 painted $APP_KITTY_PX px (Kitty) and $APP_SIXEL_PX px (Sixel)"

# ---------------------------------------------------------------------------
# Phase 15: a rejected transmit paints nothing and costs the session nothing.
# ---------------------------------------------------------------------------
REJECT_MARK=$(( $(wc -l <"$SERVER_LOG") + 1 ))
run_step rejected
assert_absent "$OUT/rejected.png" "$RED" rejected_unpainted >/dev/null
tail -n "+$REJECT_MARK" "$SERVER_LOG" | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -qF 'terminal image' \
    || fail "rejected: the server logged nothing about the over-limit transmit"
run_step ready rejected-recovered
echo "PHASE 15 PASS: an over-limit transmit painted nothing and left the pane usable"

# ---------------------------------------------------------------------------
# Phase 16: the master switch. Rolling back retires the scene and leaves the
# terminal's own text untouched.
#
# What the switch promises is a retirement, not a repaint: the session's PTY
# reader drops the retained bytes and performs the canonical reset a hard
# terminal reset performs, so no committed scene survives to reach a later
# viewer. The viewer that watched the transfer keeps the frame it was already
# given, so the pixel assertion belongs to a client that attaches afterwards —
# which is also the only one that could have been handed a scene the server was
# told to stop keeping.
# ---------------------------------------------------------------------------
run_step rgb disabled-before
assert_present "$OUT/disabled-before.png" "$RED" "$IMAGE_PX_MIN" disabled_precondition >/dev/null
SWITCH_MARK=$(( $(wc -l <"$SERVER_LOG") + 1 ))
printf '[terminal.images]\nenabled = false\n' >"$XDG_CONFIG_HOME/scribe/config.toml"
wait_client_log "config hot-reloaded" 20 || fail "the client never reloaded the disabled config"
SWITCH_LOG=$(tail -n "+$SWITCH_MARK" "$SERVER_LOG" | sed 's/\x1b\[[0-9;]*m//g')
grep -qF 'terminal image master switch changed images_enabled=false' <<<"$SWITCH_LOG" \
    || fail "the server never applied terminal.images.enabled=false"
grep -qF "released terminal image state after the master switch went off session_id=$IMAGE_SESSION" \
    <<<"$SWITCH_LOG" \
    || fail "the server never released $IMAGE_SESSION's retained image state"
kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
start_client "$IMAGE_SESSION"
sleep 1.5
capture "$OUT/disabled.png"
assert_absent "$OUT/disabled.png" "$RED" disabled_cleared >/dev/null
run_step ready disabled-recovered
echo "PHASE 16 PASS: terminal.images.enabled=false retired the scene and kept the pane alive"

python3 - "$OUT/client.json" "$EVIDENCE" <<'PY'
import json
import sys

path, evidence_path = sys.argv[1:]
measurements = {}
with open(evidence_path, encoding="utf-8") as source:
    for line in source:
        label, color, pixels, kind = line.rstrip().split("\t")
        measurements[label] = {"color": color, "pixels": int(pixels), "expected": kind}

with open(path, "w", encoding="utf-8") as handle:
    json.dump(
        {"schema": 1, "platform": "linux", "surface": "running_client",
         "measurements": measurements},
        handle, indent=2, sort_keys=True,
    )
    handle.write("\n")
PY

echo "PASS: the running client paints every terminal image path"
