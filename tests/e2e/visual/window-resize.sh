#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted + visual E2E: a live window resize republishes every pane's grid.
#
# Resizing the window re-lays the panes locally from the grid band's measured
# rect, but that rect is written during *prepaint* — after the `render` that
# reacted to the resize already compared the published area against the stale
# one. With nothing scheduling a follow-up frame, the client used to re-lay the
# panes on screen and never tell the server, so every PTY kept its pre-resize
# size and applications inside them wrapped at the old column count.
#
# A unit test cannot see that: the arithmetic was always right and the missing
# piece was the frame that never happened. Every assertion here therefore comes
# from the real window or the real wire:
#
#   * the grid repaints when the window is resized (a screenshot diff);
#   * the wire tap records a fresh `Resize` carrying MORE columns and rows when
#     the window grows, and fewer when it shrinks back;
#   * the PTY itself agrees — `stty size` inside the session reports exactly the
#     cell counts the client put on the wire, which is only true if the server
#     applied the resize;
#   * and the republish converges: an idle window after the resize settles
#     produces no further `Resize` frames, so the deferred follow-up cannot
#     become a per-frame storm.
#
# Geometry alone is not enough. A pane can publish the right cell counts, drive
# the PTY to them, repaint thousands of pixels — and still show nothing, because
# a whole-pane rebuild is *state*: it paints every row as exactly `cols`
# characters and ends on an absolute CUP, so replaying it into a grid of a
# different shape autowraps the whole screen into scrollback and leaves the
# viewport blank. The stock geometry phases passed through exactly that defect.
# The content phases therefore seed five long marker commands and then assert,
# after every settle, that the client's rendered rows agree with the server's
# own screen row for row:
#
#   * the server's `scribe-test snapshot` is the oracle for what SHOULD be on
#     screen — it is the authoritative `Term`, reflowed by the resize;
#   * the client's window is read back as per-row ink, so every row the server
#     calls non-empty must carry ink in the window at the same row index;
#   * the marker's error line is 125 columns long, so it occupies one row in the
#     wide shape and two in the narrow one. A client rendering an abandoned
#     geometry keeps the old row profile and fails the comparison even though
#     its cell counts and its pixel diff are both perfect.
#
# The window is resized through the window manager (`xdotool windowsize`), the
# same path a user's drag takes, rather than through any client action. The
# content phases drive a STEPPED sequence of those calls — a drag's cadence, not
# a single jump — and send no keyboard input at all, so nothing but the resize
# pipeline can be repairing the screen.
#
# Requires the shared-pane rig plus the wire tap:
#   just e2e-visual-window-resize
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the shared-pane rig must export a created SESSION}"

# Lit pixels the grid must hold before any phase runs, measured the way
# terminal-zoom.sh measures it: an unattached window reads a few hundred, a live
# pane showing a prompt reads thousands.
INK_MIN_PIXELS="${INK_MIN_PIXELS:-1500}"
STATUS_BAR_INSET_PX="${STATUS_BAR_INSET_PX:-20}"

# The two window shapes the test drives. Both fit the 1920x1080 Xvfb screen, and
# both differ from the startup size (120x36 cells) by far more than the one cell
# a rounding wobble could account for.
GROWN_W="${GROWN_W:-1700}"
GROWN_H="${GROWN_H:-1000}"
SHRUNK_W="${SHRUNK_W:-900}"
SHRUNK_H="${SHRUNK_H:-600}"

# Differing pixels a resize must produce inside the grid. Every pane rect moves,
# so the real number is tens of thousands; a window that never re-laid yields
# almost none (the image pins SCRIBE_DISABLE_ANIMATIONS=1, so consecutive frames
# of an idle grid are byte-identical).
RESIZE_DIFF_MIN="${RESIZE_DIFF_MIN:-5000}"

POLL_TICKS="${POLL_TICKS:-20}"

# Seconds an idle window is watched for a `Resize` storm once the geometry has
# settled. The deferred republish is armed only by a rect that actually moved,
# so a settled window must produce exactly zero.
IDLE_WATCH_SECS="${IDLE_WATCH_SECS:-4}"

# ── Content-integrity constants ───────────────────────────────────
# Client chrome layout, mirrored from the client so a drift shows up as a
# failure here rather than as rows measured against the wrong band:
#   TITLEBAR_H  titlebar.rs `TITLEBAR_HEIGHT`
#   BAR_H       window_chrome.rs `STATUS_BAR_HEIGHT`
#   ROW_H_X10   terminal_element.rs `LINE_HEIGHT_RATIO` at the default font
#               size 14 (14 * 1.35 = 18.9), x10 so row tops stay integral
TITLEBAR_H=34
BAR_H=24
ROW_H_X10=189

# Pixels trimmed off each row band and off the grid's left/right edges before
# ink is counted. The focused pane paints a 2 px accent border INSIDE its rect
# (focus_border.rs) and the scrollbar overlay owns a 24 px strip on the right;
# neither is grid text, and both would otherwise light up rows the server calls
# empty.
ROW_INSET_PX=3
GRID_INSET_L=4
GRID_INSET_R=30

# Lit pixels a row band must hold to count as rendered. A row of grid text
# reads in the thousands and the shortest row the seed produces (the shell
# prompt, ~10 glyphs) still reads in the hundreds; an empty row reads 0, so the
# bar only has to clear a lone block cursor's ~100.
ROW_INK_MIN="${ROW_INK_MIN:-80}"

# Rows the client may light that the server calls empty. The block cursor is
# one such row by construction, and a partially repainted band on the frame the
# screenshot caught can be another.
ROW_LIT_SLACK="${ROW_LIT_SLACK:-2}"

# The two shapes the content drag runs between. Wide is 1500x950 (178x45 cells)
# so the whole seed fits unwrapped; narrow is 900x620 (107x28 cells), under the
# marker's 125-column error line, so every marker reflows onto two rows and the
# server's row profile genuinely changes.
CONTENT_WIDE_W="${CONTENT_WIDE_W:-1500}"
CONTENT_WIDE_H="${CONTENT_WIDE_H:-950}"
CONTENT_NARROW_W="${CONTENT_NARROW_W:-900}"
CONTENT_NARROW_H="${CONTENT_NARROW_H:-620}"

# Steps a stepped drag is broken into, and the pause between them. Eight steps
# at 120 ms is a slow drag: each one lands its own configure event, so the
# client is asked to re-lay and republish while the previous round trip is
# still in flight — the window in which a rebuild can arrive at a shape the
# client has already left.
DRAG_STEPS="${DRAG_STEPS:-8}"
DRAG_STEP_SLEEP="${DRAG_STEP_SLEEP:-0.12}"

# Marker commands seeded into the pane. Each token is 100 characters, so the
# shell's "command not found" line is 125 columns — one row at 178 columns, two
# at 107.
MARKER_COUNT="${MARKER_COUNT:-5}"
MARKER_PAD_LEN=92

# Non-empty rows the server must be holding before a content assertion means
# anything. Five markers echo and fail, so the wide shape carries eleven.
CONTENT_ROWS_MIN="${CONTENT_ROWS_MIN:-10}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

fail() {
    echo "$1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" 2>/dev/null >&2 || true
    echo "--- server log tail ---" >&2
    tail -20 "$SERVER_LOG" 2>/dev/null >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "FAIL: no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.5
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Capture the client window only. A full-screen scrot also catches openbox's
# title bar, whose pixels belong to no phase here.
capture() {
    focus
    sleep 0.4
    scrot -o /output/resize-fullscreen.png
    convert /output/resize-fullscreen.png \
        -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$1"
}

window_ink() {
    local value
    value=$(convert "$1" \
        -gravity North -crop "${WIN_W}x$(( WIN_H - STATUS_BAR_INSET_PX ))+0+0" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Differing pixels between two captures over the region they share. The frames
# compared here have different dimensions on purpose (that is the resize), so
# the diff is taken over the smaller frame's box anchored at the top-left.
frame_diff() {
    local w h value
    w=$(convert "$1" -format "%w" info:)
    h=$(convert "$1" -format "%h" info:)
    local w2 h2
    w2=$(convert "$2" -format "%w" info:)
    h2=$(convert "$2" -format "%h" info:)
    [ "$w2" -lt "$w" ] && w="$w2"
    [ "$h2" -lt "$h" ] && h="$h2"
    value=$(compare -metric AE \
        \( "$1" -crop "${w}x${h}+0+0" +repage \) \
        \( "$2" -crop "${w}x${h}+0+0" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

# Recorded client→server `Resize` frames for this session.
count_resizes() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
total = 0
try:
    handle = open(path)
except OSError:
    print(0)
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "Resize":
            continue
        if str(message.get("session_id")) != session:
            continue
        total += 1
print(total)
PY
}

# Geometry of the newest recorded `Resize` for this session, as "cols rows".
# Empty when the client has published none.
last_resize() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
newest = None
try:
    handle = open(path)
except OSError:
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "Resize":
            continue
        if str(message.get("session_id")) != session:
            continue
        newest = message.get("size", {})
if newest is not None:
    print(newest.get("cols", 0), newest.get("rows", 0))
PY
}

# Wait until another `Resize` for this session lands on the wire.
wait_for_resize() {
    local baseline="$1" timeout_secs="${2:-20}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_resizes)" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Ask the PTY what size the server gave it. `stty` is answered by the kernel's
# window size, so a match proves the `Resize` was applied end to end and not
# merely written to a socket.
assert_pty_size() {
    local marker="$1" rows="$2" cols="$3"
    # Exactly one `\n` in the payload: `scribe-test send` translates escapes, so
    # a second one would break the command line in half before the shell ran it.
    scribe-test send "$SESSION" "echo ${marker}=\$(stty size | tr ' ' 'x')\n"
    scribe-test wait-output "$SESSION" "${marker}=${rows}x${cols}" --timeout 15000 >/dev/null \
        || return 1
    return 0
}

# Capture the client window's own pixels. `import -window` asks the X server
# for that window's contents, so every offset below is window-relative and no
# WM decoration can shift a row band.
shot_window() {
    focus
    sleep 0.4
    import -window "$(find_window)" +repage "$1"
}

# Walk the window from one shape to another in `DRAG_STEPS` `xdotool
# windowsize` calls. Nothing else touches the window: no keys, no clicks.
drag_window() {
    local from_w="$1" from_h="$2" to_w="$3" to_h="$4" step w h
    for step in $(seq 1 "$DRAG_STEPS"); do
        w=$(( from_w + (to_w - from_w) * step / DRAG_STEPS ))
        h=$(( from_h + (to_h - from_h) * step / DRAG_STEPS ))
        xdotool windowsize "$WID" "$w" "$h"
        sleep "$DRAG_STEP_SLEEP"
    done
}

# The marker token for index $1: a fixed prefix padded out to 100 characters.
marker_token() {
    local pad
    pad=$(printf 'x%.0s' $(seq 1 "$MARKER_PAD_LEN"))
    printf 'RSZMARK%s_%s' "$1" "$pad"
}

# Refresh the server's cached screen for this session and write it to $1.
#
# The daemon answers a snapshot request from its cached `latest_snapshot` and
# only *then* replaces it with the one it just asked the server for, so a single
# call returns the previous screen. Two calls therefore read the current one.
server_snapshot() {
    scribe-test snapshot "$SESSION" "$1" >/dev/null
    scribe-test snapshot "$SESSION" "$1" >/dev/null
}

# Poll the server's screen until it is `$2`x`$3` cells and still holds the last
# seeded marker, leaving the JSON in $1. The geometry gate is what makes the
# comparison that follows meaningful: comparing the client against a screen the
# server has not reflowed yet would fail a correct client.
wait_for_server_screen() {
    local out="$1" cols="$2" rows="$3" timeout_secs="${4:-25}" started needle
    needle="RSZMARK${MARKER_COUNT}_"
    started=$(date +%s)
    while true; do
        server_snapshot "$out"
        if python3 - "$out" "$cols" "$rows" "$needle" <<'PY'
import json, sys

path, cols, rows, needle = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
try:
    with open(path, encoding="utf-8") as handle:
        snap = json.load(handle)
except (OSError, ValueError):
    sys.exit(1)
if snap.get("cols") != cols or snap.get("rows") != rows:
    sys.exit(1)
text = "".join(cell.get("c", " ") for cell in snap.get("cells", []))
sys.exit(0 if needle in text else 1)
PY
        then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.5
    done
}

# Compare the client's rendered grid against the server's screen, row by row.
#
# $1 label, $2 output tag, $3 expected cols, $4 expected rows. Prints the two
# row profiles and the lit-row counts; returns non-zero with a diagnosis when
# they disagree.
assert_grid_matches_server() {
    local label="$1" tag="$2" cols="$3" rows="$4"
    local snap="/output/resize-${tag}-server.json"
    local shot="/output/resize-${tag}-client.png"
    local grid="/output/resize-${tag}-grid.gray"

    wait_for_server_screen "$snap" "$cols" "$rows" \
        || fail "$label: the server never settled at ${cols}x${rows} holding the seeded markers"

    shot_window "$shot"
    local grid_w grid_h
    grid_w=$(( WIN_W - GRID_INSET_L - GRID_INSET_R ))
    grid_h=$(( rows * ROW_H_X10 / 10 ))
    if [ "$grid_w" -le 0 ] \
        || [ "$grid_h" -gt $(( WIN_H - TITLEBAR_H - BAR_H )) ]; then
        fail "$label: a ${rows}-row grid does not fit the ${WIN_W}x${WIN_H} window"
    fi
    # One thresholded greyscale crop of the grid band, written as raw bytes
    # (`gray:`) so there is no image header to parse and the row arithmetic can
    # be done at full precision on a fractional row height.
    convert "$shot" \
        -crop "${grid_w}x${grid_h}+${GRID_INSET_L}+${TITLEBAR_H}" +repage \
        -colorspace Gray -threshold 35% -depth 8 "gray:${grid}"

    python3 - "$grid" "$grid_w" "$grid_h" "$snap" "$rows" "$ROW_H_X10" \
        "$ROW_INSET_PX" "$ROW_INK_MIN" "$ROW_LIT_SLACK" "$CONTENT_ROWS_MIN" \
        "$label" <<'PY'
import json
import sys

(
    grid_path,
    width_arg,
    height_arg,
    snap_path,
    rows_arg,
    row_h_x10,
    inset,
    ink_min,
    lit_slack,
    rows_min,
    label,
) = sys.argv[1:12]
width = int(width_arg)
height = int(height_arg)
rows = int(rows_arg)
row_h_x10 = int(row_h_x10)
inset = int(inset)
ink_min = int(ink_min)
lit_slack = int(lit_slack)
rows_min = int(rows_min)

# `gray:` is one byte per pixel, row-major, no header.
with open(grid_path, "rb") as handle:
    pixels = handle.read()
if len(pixels) != width * height:
    raise SystemExit(
        f"{label}: {grid_path} holds {len(pixels)} bytes, expected"
        f" {width * height} for a {width}x{height} crop"
    )

client_ink = []
for row in range(rows):
    top = row * row_h_x10 // 10 + inset
    bottom = (row + 1) * row_h_x10 // 10 - inset
    top = max(top, 0)
    bottom = min(bottom, height)
    # The crop was thresholded, so every byte is 0 or 255 and "lit" is just
    # "not background".
    lit = 0
    for y in range(top, bottom):
        band = pixels[y * width : (y + 1) * width]
        lit += len(band) - band.count(0)
    client_ink.append(lit)

with open(snap_path, encoding="utf-8") as handle:
    snap = json.load(handle)
cols = snap["cols"]
cells = snap["cells"]
server_lit = []
for row in range(rows):
    line = "".join(
        cell.get("c", " ") for cell in cells[row * cols : (row + 1) * cols]
    )
    server_lit.append(bool(line.strip()))

client_lit = [ink >= ink_min for ink in client_ink]
profile = lambda flags: "".join("#" if flag else "." for flag in flags)
print(f"  {label}: server rows {profile(server_lit)}")
print(f"  {label}: client rows {profile(client_lit)}")
print(
    f"  {label}: {sum(server_lit)} server / {sum(client_lit)} client rows lit"
    f" over {rows} rows"
)

problems = []
if sum(server_lit) < rows_min:
    problems.append(
        f"the server screen holds only {sum(server_lit)} non-empty rows"
        f" (need {rows_min}) — the seed never landed"
    )
missing = [row for row in range(rows) if server_lit[row] and not client_lit[row]]
if missing:
    detail = ", ".join(f"{row}({client_ink[row]}px)" for row in missing[:8])
    problems.append(
        f"{len(missing)} row(s) the server is showing are blank in the window:"
        f" {detail}"
    )
ghosts = [row for row in range(rows) if client_lit[row] and not server_lit[row]]
if len(ghosts) > lit_slack:
    detail = ", ".join(f"{row}({client_ink[row]}px)" for row in ghosts[:8])
    problems.append(
        f"{len(ghosts)} row(s) carry ink the server's screen does not have"
        f" (slack {lit_slack}): {detail}"
    )
if problems:
    for problem in problems:
        print(f"  {label}: {problem}", file=sys.stderr)
    sys.exit(1)
print(f"  {label}: every server row is on screen and nothing extra is")
PY
}

# ── Phase 0: the shared pane is painted ───────────────────────────
ink=0
for _ in $(seq 1 "$POLL_TICKS"); do
    capture /output/resize-00-attached.png
    ink=$(window_ink /output/resize-00-attached.png)
    [ "$ink" -ge "$INK_MIN_PIXELS" ] && break
    sleep 0.5
done
if [ "$ink" -lt "$INK_MIN_PIXELS" ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content ($ink lit px)"
fi
echo "PHASE 0 PASS: the client is attached to session $SESSION ($ink lit px)"

# ── Phase 1: the startup geometry is on the wire and in the PTY ───
if ! wait_for_resize 0 25; then
    fail "PHASE 1 FAIL: the client published no pane geometry at all"
fi
sleep 1.0
read -r BASE_COLS BASE_ROWS <<<"$(last_resize)"
capture /output/resize-01-baseline.png
assert_pty_size PTYBASE "$BASE_ROWS" "$BASE_COLS" \
    || fail "PHASE 1 FAIL: the PTY does not report the published ${BASE_COLS}x${BASE_ROWS}"
echo "PHASE 1 PASS: startup geometry is ${BASE_COLS}x${BASE_ROWS} on the wire and in the PTY"

# ── Phase 2: growing the window republishes a bigger grid ─────────
RESIZE_BASE=$(count_resizes)
WID=$(find_window)
xdotool windowsize "$WID" "$GROWN_W" "$GROWN_H"
if ! wait_for_resize "$RESIZE_BASE" 25; then
    fail "PHASE 2 FAIL: the window grew to ${GROWN_W}x${GROWN_H} but no Resize reached the wire"
fi
sleep 1.0
read -r GROWN_COLS GROWN_ROWS <<<"$(last_resize)"
capture /output/resize-02-grown.png
DIFF=$(frame_diff /output/resize-01-baseline.png /output/resize-02-grown.png)
if [ "${DIFF:-0}" -lt "$RESIZE_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: the grown window differs by only $DIFF px (min $RESIZE_DIFF_MIN)"
fi
if [ "$GROWN_COLS" -le "$BASE_COLS" ] || [ "$GROWN_ROWS" -le "$BASE_ROWS" ]; then
    fail "PHASE 2 FAIL: growing the window published ${GROWN_COLS}x${GROWN_ROWS}, not above ${BASE_COLS}x${BASE_ROWS}"
fi
assert_pty_size PTYGROWN "$GROWN_ROWS" "$GROWN_COLS" \
    || fail "PHASE 2 FAIL: the PTY kept its old size instead of ${GROWN_COLS}x${GROWN_ROWS}"
echo "PHASE 2 PASS: the grown window re-laid the grid (+$DIFF px) and published ${GROWN_COLS}x${GROWN_ROWS} to the PTY"

# ── Phase 3: shrinking republishes a smaller grid ─────────────────
RESIZE_BASE=$(count_resizes)
xdotool windowsize "$WID" "$SHRUNK_W" "$SHRUNK_H"
if ! wait_for_resize "$RESIZE_BASE" 25; then
    fail "PHASE 3 FAIL: the window shrank to ${SHRUNK_W}x${SHRUNK_H} but no Resize reached the wire"
fi
sleep 1.0
read -r SHRUNK_COLS SHRUNK_ROWS <<<"$(last_resize)"
capture /output/resize-03-shrunk.png
if [ "$SHRUNK_COLS" -ge "$GROWN_COLS" ] || [ "$SHRUNK_ROWS" -ge "$GROWN_ROWS" ]; then
    fail "PHASE 3 FAIL: shrinking published ${SHRUNK_COLS}x${SHRUNK_ROWS}, not below ${GROWN_COLS}x${GROWN_ROWS}"
fi
assert_pty_size PTYSHRUNK "$SHRUNK_ROWS" "$SHRUNK_COLS" \
    || fail "PHASE 3 FAIL: the PTY kept ${GROWN_COLS}x${GROWN_ROWS} instead of ${SHRUNK_COLS}x${SHRUNK_ROWS}"
echo "PHASE 3 PASS: the shrunk window published ${SHRUNK_COLS}x${SHRUNK_ROWS} to the PTY"

# ── Phase 4: a settled window publishes nothing further ───────────
SETTLED=$(count_resizes)
sleep "$IDLE_WATCH_SECS"
AFTER=$(count_resizes)
if [ "$AFTER" -ne "$SETTLED" ]; then
    fail "PHASE 4 FAIL: an idle window published $(( AFTER - SETTLED )) more Resize frames in ${IDLE_WATCH_SECS}s"
fi
echo "PHASE 4 PASS: the republish converged — no Resize frames from a settled window"

# ── Phase 5: a settled window with no resize renders the whole screen ──
# The zero-resize baseline. Everything from here on compares the window against
# the server's own screen, and this phase establishes that the comparison holds
# when nothing has been dragged at all — a pane that is already corrupt at rest
# would otherwise make every later phase look like a resize defect.
xdotool windowsize "$WID" "$CONTENT_WIDE_W" "$CONTENT_WIDE_H"
sleep 2.0
read -r WIDE_COLS WIDE_ROWS <<<"$(last_resize)"
[ -n "$WIDE_COLS" ] && [ "$WIDE_COLS" -gt 0 ] \
    || fail "PHASE 5 FAIL: the client published no geometry for the ${CONTENT_WIDE_W}x${CONTENT_WIDE_H} window"

# `clear` puts the seed at the top of the screen, so a row index in the client
# window and a row index in the server's snapshot mean the same thing.
scribe-test send "$SESSION" "clear\n"
sleep 0.5
for index in $(seq 1 "$MARKER_COUNT"); do
    MARKER=$(marker_token "$index")
    scribe-test send "$SESSION" "${MARKER}\n"
    scribe-test wait-output "$SESSION" "$MARKER" --timeout 15000 >/dev/null \
        || fail "PHASE 5 FAIL: marker $index never reached the pane"
done
sleep 1.5

assert_grid_matches_server "PHASE 5" 05-seeded "$WIDE_COLS" "$WIDE_ROWS" \
    || fail "PHASE 5 FAIL: the settled window does not show the screen the server has"
WIDE_LIT=$(python3 - "/output/resize-05-seeded-server.json" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as handle:
    snap = json.load(handle)
cols, rows, cells = snap["cols"], snap["rows"], snap["cells"]
print(sum(
    1
    for row in range(rows)
    if "".join(c.get("c", " ") for c in cells[row * cols:(row + 1) * cols]).strip()
))
PY
)
echo "PHASE 5 PASS: ${MARKER_COUNT} markers render row for row at ${WIDE_COLS}x${WIDE_ROWS} with no resize"

# ── Phase 6: a stepped shrink drag reflows the screen and keeps it ─────
# The drag the geometry phases never performed: successive `windowsize` calls,
# each landing its own configure event while the previous round trip is still
# in flight. Narrowing under the marker's 125-column error line forces the
# server to reflow every marker onto two rows, so a window still painting the
# shape it was rendered at cannot match the new profile by accident.
drag_window "$CONTENT_WIDE_W" "$CONTENT_WIDE_H" "$CONTENT_NARROW_W" "$CONTENT_NARROW_H"
sleep 2.5
read -r NARROW_COLS NARROW_ROWS <<<"$(last_resize)"
if [ "$NARROW_COLS" -ge "$WIDE_COLS" ]; then
    fail "PHASE 6 FAIL: the drag narrowed the window but published ${NARROW_COLS} columns, not below ${WIDE_COLS}"
fi
assert_pty_size PTYDRAG "$NARROW_ROWS" "$NARROW_COLS" \
    || fail "PHASE 6 FAIL: the PTY did not follow the stepped drag to ${NARROW_COLS}x${NARROW_ROWS}"
assert_grid_matches_server "PHASE 6" 06-narrow "$NARROW_COLS" "$NARROW_ROWS" \
    || fail "PHASE 6 FAIL: the window lost content the server is still showing after the drag"
NARROW_LIT=$(python3 - "/output/resize-06-narrow-server.json" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as handle:
    snap = json.load(handle)
cols, rows, cells = snap["cols"], snap["rows"], snap["cells"]
print(sum(
    1
    for row in range(rows)
    if "".join(c.get("c", " ") for c in cells[row * cols:(row + 1) * cols]).strip()
))
PY
)
if [ "$NARROW_LIT" -le "$WIDE_LIT" ]; then
    fail "PHASE 6 FAIL: narrowing to ${NARROW_COLS} columns did not reflow the seed ($WIDE_LIT -> $NARROW_LIT rows)"
fi
echo "PHASE 6 PASS: the stepped shrink reflowed $WIDE_LIT rows to $NARROW_LIT and the window shows all of them"

# ── Phase 7: a stepped grow drag puts the unwrapped screen back ────────
# The reverse drag matters on its own: growing replays a rebuild that is WIDER
# than the grid the client is leaving, which wraps in the opposite direction and
# is the shape a shrink-only test never produces.
drag_window "$CONTENT_NARROW_W" "$CONTENT_NARROW_H" "$CONTENT_WIDE_W" "$CONTENT_WIDE_H"
sleep 2.5
read -r REGROWN_COLS REGROWN_ROWS <<<"$(last_resize)"
if [ "$REGROWN_COLS" -le "$NARROW_COLS" ]; then
    fail "PHASE 7 FAIL: the drag widened the window but published ${REGROWN_COLS} columns, not above ${NARROW_COLS}"
fi
assert_grid_matches_server "PHASE 7" 07-regrown "$REGROWN_COLS" "$REGROWN_ROWS" \
    || fail "PHASE 7 FAIL: the window did not recover the unwrapped screen after the grow drag"
echo "PHASE 7 PASS: the stepped grow restored ${REGROWN_COLS}x${REGROWN_ROWS} with every server row on screen"

echo ""
echo "ALL PHASES PASS — a live window resize reaches the server and the PTY,"
echo "and the pane keeps showing the screen the server has throughout a drag."
echo "  Captures:    test-output/resize-0*.png"
echo "  Wire record: test-output/share-wire.jsonl"
