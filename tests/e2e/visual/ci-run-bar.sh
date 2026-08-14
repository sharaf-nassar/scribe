#!/bin/bash
# e2e-timeout: 180
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
set -euo pipefail

TITLEBAR_H=34
BAR_H=40
GRID_PROBE_W=760
STATE_W=110
STATE_CHANGE_MIN=20
API_PORT=8098
API_URL="http://127.0.0.1:$API_PORT"
API_LOG=/output/ci-bar-api.jsonl
API_SERVER_LOG=/output/ci-bar-api.log
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
REPO=/tmp/ci-bar-repo
REMOTE=/tmp/ci-bar-remote.git
SCENARIO=/tmp/github-ci-run-bar.json
API_PID=""
WID=""
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

cleanup() {
    [ -z "$API_PID" ] || kill "$API_PID" 2>/dev/null || true
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    tail -60 "$SERVER_LOG" >&2 2>/dev/null || true
    tail -40 "$CLIENT_LOG" >&2 2>/dev/null || true
    exit 1
}

focus() {
    local info
    WID=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -n "$WID" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    # openbox reparents the window, so xdotool's coordinates miss its top
    # client pixels. xwininfo reports the drawable client box used below.
    info=$(xwininfo -id "$WID")
    WIN_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    WIN_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    WIN_W=$(printf '%s\n' "$info" | awk '/Width:/ { print $2 }')
    WIN_H=$(printf '%s\n' "$info" | awk '/Height:/ { print $2 }')
}

capture() {
    local target="$1" screen=/tmp/ci-bar-screen.png
    focus
    sleep 0.25
    scrot -o "$screen"
    convert "$screen" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage "$target"
}

plain_client_log() {
    sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"
}

latest_grid() {
    plain_client_log | python3 -c '
import re, sys
lines = [line for line in sys.stdin if "published a pane\047s grid size" in line]
match = re.search(r"cols=(\d+).*rows=(\d+)", lines[-1]) if lines else None
print(match.group(1), match.group(2)) if match else print("0 0")
'
}

snapshot_text() {
    local json="$1" text="$2"
    scribe-test snapshot "$SESSION" "$json" >/dev/null
    scribe-test snapshot "$SESSION" "$json" >/dev/null
    python3 - "$json" >"$text" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    screen = json.load(handle)
cols = screen.get("cols") or 1
cells = [cell.get("c", " ") for cell in screen.get("cells", [])]
for start in range(0, len(cells), cols):
    row = "".join(cells[start:start + cols]).rstrip()
    if row.startswith("CI_FRAME_"):
        print(row)
PY
}

request_count() {
    wc -l <"$API_LOG" 2>/dev/null || printf '0\n'
}

wait_requests() {
    local expected="$1"
    for _ in $(seq 1 60); do
        [ "$(request_count)" -ge "$expected" ] && return 0
        sleep 0.25
    done
    fail "API request $expected did not arrive"
}

state_mask() {
    convert "$1" -crop "${STATE_W}x${BAR_H}+0+${TITLEBAR_H}" +repage \
        -colorspace Gray -threshold 12% "$2"
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

mask_ink() {
    local value
    value=$(convert "$1" -crop "$2" +repage -format '%[fx:mean*w*h]' info:)
    printf '%s' "${value%.*}"
}

assert_state_cues() {
    local label="$1" mask="$2" glyph word
    glyph=$(mask_ink "$mask" '18x24+10+8')
    word=$(mask_ink "$mask" '76x24+30+8')
    [ "${glyph:-0}" -ge 8 ] || fail "$label lost its non-color state glyph ($glyph px)"
    [ "${word:-0}" -ge 20 ] || fail "$label lost its non-color state word ($word px)"
}

wait_state_change() {
    local label="$1" previous="$2" target="$3" mask="$4" changed=0
    for _ in $(seq 1 40); do
        capture "$target"
        state_mask "$target" "$mask"
        changed=$(pixel_diff "$previous" "$mask")
        [ "$changed" -ge "$STATE_CHANGE_MIN" ] && return 0
        sleep 0.25
    done
    fail "$label did not replace its non-color state cue ($changed changed px)"
}

push_head() {
    local branch="$1"
    git -C "$REPO" remote set-url origin "$REMOTE"
    git -C "$REPO" push origin "$branch:main" >/dev/null
    # The local bare remote keeps Docker offline. The watched logical push ref
    # is then attributed to the github.com URL production would retain.
    git -C "$REPO" remote set-url origin git@github.com:acme/widget.git
}

[ "${SCRIBE_GITHUB_API_URL:-}" = "$API_URL" ] \
    || fail "loopback GitHub API URL was not passed into the container"
ip route show default | grep -q . \
    && fail "container has a default route despite --network none"

git init --bare "$REMOTE" >/dev/null
git init "$REPO" >/dev/null
git -C "$REPO" config user.email scribe@example.com
git -C "$REPO" config user.name 'Scribe E2E'
git -C "$REPO" config core.hooksPath .git-hooks-disabled
git -C "$REPO" remote add origin "$REMOTE"
git -C "$REPO" commit --allow-empty -m running >/dev/null
git -C "$REPO" branch state-running
git -C "$REPO" commit --allow-empty -m failure >/dev/null
git -C "$REPO" branch state-failure
git -C "$REPO" commit --allow-empty -m cancelled >/dev/null
git -C "$REPO" branch state-cancelled
git -C "$REPO" commit --allow-empty -m stale >/dev/null
git -C "$REPO" branch state-stale

RUNNING_HEAD=$(git -C "$REPO" rev-parse state-running)
FAILURE_HEAD=$(git -C "$REPO" rev-parse state-failure)
CANCELLED_HEAD=$(git -C "$REPO" rev-parse state-cancelled)
STALE_HEAD=$(git -C "$REPO" rev-parse state-stale)
python3 - /tests/fixtures/github-ci-run-bar.json "$SCENARIO" \
    "$RUNNING_HEAD" "$FAILURE_HEAD" "$CANCELLED_HEAD" "$STALE_HEAD" <<'PY'
from pathlib import Path
import sys

source, target, *heads = sys.argv[1:]
text = Path(source).read_text(encoding="utf-8")
for token, head in zip(
    ("__RUNNING_HEAD__", "__FAILURE_HEAD__", "__CANCELLED_HEAD__", "__STALE_HEAD__"),
    heads,
):
    text = text.replace(token, head)
Path(target).write_text(text, encoding="utf-8")
PY

scribe-test github-actions-api --scenario "$SCENARIO" --request-log "$API_LOG" \
    --port "$API_PORT" >"$API_SERVER_LOG" 2>&1 &
API_PID=$!
for _ in $(seq 1 50); do
    grep -q "listening on 127.0.0.1:$API_PORT" "$API_SERVER_LOG" 2>/dev/null && break
    kill -0 "$API_PID" 2>/dev/null || fail "GitHub API fixture exited"
    sleep 0.1
done
grep -q "listening on 127.0.0.1:$API_PORT" "$API_SERVER_LOG" \
    || fail "GitHub API fixture did not bind loopback"

scribe-test send "$SESSION" "cd '$REPO'; printf '\\033]0;ci-bar-cwd\\007CI_CWD_READY\\n'\n"
scribe-test wait-output "$SESSION" CI_CWD_READY
# Docker's visual image intentionally lacks the installed shell-integration
# assets. OSC 0 drives the production /proc child-CWD fallback used by shells
# without OSC 7, then the returned prompt leaves that observation settled.
sleep 1
scribe-test send "$SESSION" "printf '\\033[?25l\\033[2J\\033[HCI_FRAME_01 alpha\\nCI_FRAME_02 beta\\nCI_FRAME_03 gamma\\nCI_FRAME_04 delta\\nCI_FRAME_05 epsilon\\nCI_FRAME_06 zeta\\nCI_FRAME_07 eta\\nCI_FRAME_08 theta\\n'; sleep 300\n"
scribe-test wait-output "$SESSION" CI_FRAME_08
sleep 1

capture /output/ci-bar-00-baseline.png
read -r BASE_COLS BASE_ROWS <<<"$(latest_grid)"
[ "$BASE_ROWS" -gt 0 ] || fail "client published no baseline grid"
snapshot_text /output/ci-bar-00-baseline-pty.json /output/ci-bar-00-baseline-pty.txt
[ "$(wc -l </output/ci-bar-00-baseline-pty.txt)" -eq 8 ] \
    || fail "baseline PTY lost frame markers"

push_head state-running
wait_requests 1
for _ in $(seq 1 40); do
    read -r ACTIVE_COLS ACTIVE_ROWS <<<"$(latest_grid)"
    [ "$ACTIVE_ROWS" -lt "$BASE_ROWS" ] && break
    sleep 0.25
done
[ "${ACTIVE_ROWS:-$BASE_ROWS}" -lt "$BASE_ROWS" ] \
    || fail "running bar reserved no PTY rows ($BASE_ROWS -> ${ACTIVE_ROWS:-$BASE_ROWS})"
capture /output/ci-bar-01-running.png
state_mask /output/ci-bar-01-running.png /output/ci-bar-01-running-mask.png
assert_state_cues running /output/ci-bar-01-running-mask.png

# @lat: [[test#Visual E2E Tests#Collapsed CI run bar visual contract#Forty-pixel frame stability]]
[ "$ACTIVE_COLS" -eq "$BASE_COLS" ] \
    || fail "CI bar changed terminal columns ($BASE_COLS -> $ACTIVE_COLS)"
# The shared-pane rig's participant card is fixed to the window's right edge;
# the first 760px are unobscured terminal cells in the default 1008px window.
[ "$WIN_W" -gt "$GRID_PROBE_W" ] || fail "window is too narrow for terminal frame probe"
convert /output/ci-bar-00-baseline.png \
    -crop "${GRID_PROBE_W}x160+0+${TITLEBAR_H}" +repage /tmp/ci-grid-before.png
convert /output/ci-bar-01-running.png \
    -crop "${GRID_PROBE_W}x160+0+$(( TITLEBAR_H + BAR_H ))" +repage /tmp/ci-grid-after.png
GRID_SHIFT_DIFF=$(pixel_diff /tmp/ci-grid-before.png /tmp/ci-grid-after.png)
[ "$GRID_SHIFT_DIFF" -eq 0 ] \
    || fail "terminal pixels changed after ${BAR_H}px reflow ($GRID_SHIFT_DIFF px)"
snapshot_text /output/ci-bar-01-running-pty.json /output/ci-bar-01-running-pty.txt
diff -u /output/ci-bar-00-baseline-pty.txt /output/ci-bar-01-running-pty.txt \
    || fail "terminal cell content changed when the CI bar appeared"

# The approved trace mock fixes a #101013 recessed band and a one-pixel
# ownership underline. Theme conversion can round one channel, hence ±4.
read -r BAND_R BAND_G BAND_B <<<"$(convert /output/ci-bar-01-running.png -format \
    '%[fx:round(255*p{4,54}.r)] %[fx:round(255*p{4,54}.g)] %[fx:round(255*p{4,54}.b)]' info:)"
for sample in "$BAND_R:16" "$BAND_G:16" "$BAND_B:19"; do
    actual=${sample%%:*} expected=${sample##*:}
    [ "$actual" -ge "$(( expected - 4 ))" ] && [ "$actual" -le "$(( expected + 4 ))" ] \
        || fail "band background ($BAND_R,$BAND_G,$BAND_B) drifted from mockup #101013"
done
UNDERLINE_DELTA=$(convert /output/ci-bar-01-running.png -format \
    '%[fx:round(255*(abs(p{500,72}.r-p{500,73}.r)+abs(p{500,72}.g-p{500,73}.g)+abs(p{500,72}.b-p{500,73}.b)))]' info:)
[ "$UNDERLINE_DELTA" -ge 3 ] || fail "40px bar has no one-pixel ownership underline"

wait_requests 2
wait_state_change passed /output/ci-bar-01-running-mask.png \
    /output/ci-bar-02-passed.png /output/ci-bar-02-passed-mask.png
assert_state_cues passed /output/ci-bar-02-passed-mask.png

push_head state-failure
wait_requests 3
wait_state_change failed /output/ci-bar-02-passed-mask.png \
    /output/ci-bar-03-failed.png /output/ci-bar-03-failed-mask.png
assert_state_cues failed /output/ci-bar-03-failed-mask.png

push_head state-cancelled
wait_requests 4
wait_state_change cancelled /output/ci-bar-03-failed-mask.png \
    /output/ci-bar-04-cancelled.png /output/ci-bar-04-cancelled-mask.png
assert_state_cues cancelled /output/ci-bar-04-cancelled-mask.png

push_head state-stale
wait_requests 5
wait_state_change running-stale-head /output/ci-bar-04-cancelled-mask.png \
    /output/ci-bar-05-running-before-stale.png \
    /output/ci-bar-05-running-before-stale-mask.png
assert_state_cues running /output/ci-bar-05-running-before-stale-mask.png
kill "$API_PID"
wait "$API_PID" 2>/dev/null || true
API_PID=""
for _ in $(seq 1 50); do
    grep -qF 'GitHub CI request failed; retrying with bounded backoff' "$SERVER_LOG" && break
    sleep 0.25
done
grep -qF 'GitHub CI request failed; retrying with bounded backoff' "$SERVER_LOG" \
    || fail "loopback outage did not reach the active tracker"
wait_state_change stale /output/ci-bar-05-running-before-stale-mask.png \
    /output/ci-bar-06-stale.png /output/ci-bar-06-stale-mask.png
assert_state_cues stale /output/ci-bar-06-stale-mask.png

# @lat: [[test#Visual E2E Tests#Collapsed CI run bar visual contract#Public push state progression]]
[ "$(request_count)" -eq 5 ] || fail "fixture saw $(request_count) requests instead of 5"
for pair in \
    '01-running 02-passed' \
    '02-passed 03-failed' \
    '03-failed 04-cancelled' \
    '05-running-before-stale 06-stale'; do
    read -r left right <<<"$pair"
    delta=$(pixel_diff "/output/ci-bar-$left-mask.png" "/output/ci-bar-$right-mask.png")
    [ "$delta" -ge "$STATE_CHANGE_MIN" ] \
        || fail "$left and $right differ by only $delta non-color cue pixels"
done

echo "PASS: collapsed CI bar follows real pushes through running, passed, failed," \
    "cancelled, and stale; ${BAR_H}px reflow preserved ${BASE_COLS} columns and terminal pixels"
