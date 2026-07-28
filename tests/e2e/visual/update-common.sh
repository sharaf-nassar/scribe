#!/bin/bash
# Shared rig for the two update-surface visual E2E tests.
#
# Both tests drive the same first phases — stand up the fake releases API, wait
# for the real server's periodic check to broadcast `UpdateAvailable`, prove the
# centred status-bar CTA appeared in pixels, and click it — then diverge on
# which button of the confirmation modal they activate. Everything common lives
# here.
#
# Requires: visual container with --gpus all, xdotool, scrot, imagemagick,
# python3, and the container started with
#   -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest
#   -e SCRIBE_EXTRA_CONFIG='[terminal.status_bar_stats]\ncpu = false\n…'
# so `scribe-server` polls the fake API instead of GitHub and the status bar's
# live sparklines do not churn the band being diffed.

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
API_PID=""

# Window size the rig drives. Both bottom bands are on screen at the default
# window size now that it is derived from the grid plus the chrome bands (see
# crates/scribe-client/src/window_chrome.rs); the rig still grows the
# window so the status bar's left and right groups spread apart and leave the
# centred CTA — the only band these tests diff — clear space of its own.
WINDOW_W=1280
WINDOW_H=860

cleanup_update_rig() {
    if [ -n "$API_PID" ]; then
        kill "$API_PID" 2>/dev/null || true
    fi
}
trap cleanup_update_rig EXIT

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | head -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | head -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool windowactivate --sync "$wid" 2>/dev/null \
            || xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.3
    fi
}

shot() {
    focus
    sleep 0.2
    scrot -o "$1"
    echo "captured $1"
}

# Real XTEST key events to the focused window, so the client sees them exactly
# as it would a physical keypress.
send_keys() {
    focus
    xdotool key "$@"
    sleep 0.4
}

resize_window() {
    local wid
    wid=$(find_window)
    [ -z "$wid" ] && return 1
    xdotool windowsize "$wid" "$WINDOW_W" "$WINDOW_H"
    sleep 1.5
    focus
}

# Echo "W H OX OY" for the Scribe window frame as it appears in `shot`s, found
# by trimming the black Xvfb root away. `xdotool getwindowgeometry` reports
# pre-reparenting coordinates under openbox and does not agree with the
# screenshot, so the screenshot itself is the source of truth.
window_bbox() {
    convert "$1" -bordercolor black -fuzz 1% -trim -format "%w %h %X %Y" info: | tr -d '+'
}

# Crop the status-bar row's centre out of a full screenshot. The outer fifths
# are dropped so the connection dot on the left and the host label on the right
# never enter the comparison; only the centred CTA lives in what remains.
crop_cta_band() {
    local src="$1" dest="$2" w h ox oy
    read -r w h ox oy <<<"$(window_bbox "$src")"
    convert "$src" \
        -crop "$((w * 6 / 10))x34+$((ox + w / 5))+$((oy + h - 38))" +repage "$dest"
    printf '%s %s' "$((ox + w / 5))" "$((oy + h - 38))" >/tmp/cta-band-origin
}

# Count differing pixels between two CTA-band crops.
cta_band_delta() {
    local diff
    diff=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    echo "${diff%%.*}"
}

# Absolute root coordinates of the centre of whatever changed between the two
# CTA-band crops — i.e. of the update CTA itself. Echoes "X Y".
cta_click_point() {
    local before="$1" after="$2" bx by bw bh ox oy
    compare "$before" "$after" -metric AE -highlight-color white -lowlight-color black \
        -compose src /output/cta-diff.png 2>/dev/null || true
    read -r bw bh bx by <<<"$(convert /output/cta-diff.png -bordercolor black -fuzz 20% \
        -trim -format "%w %h %X %Y" info: | tr -d '+')"
    read -r ox oy </tmp/cta-band-origin
    printf '%s %s' "$((ox + bx + bw / 2))" "$((oy + by + bh / 2))"
}

# Block until `pattern` appears in `file`, or fail after `timeout_secs`.
wait_for_log() {
    local file="$1" pattern="$2" timeout_secs="${3:-60}" started
    started=$(date +%s)
    while true; do
        if grep -q -- "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        if [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ]; then
            echo "timed out waiting for '$pattern' in $file" >&2
            tail -30 "$file" >&2 2>/dev/null || true
            return 1
        fi
        sleep 0.5
    done
}

start_fake_update_api() {
    python3 /tests/visual/fake-update-api.py >/output/fake-update-api.log 2>&1 &
    API_PID=$!
    local started
    started=$(date +%s)
    while ! grep -q "serving v" /output/fake-update-api.log 2>/dev/null; do
        if [ $(("$(date +%s)" - started)) -ge 15 ]; then
            echo "fake update API did not start" >&2
            cat /output/fake-update-api.log >&2 2>/dev/null || true
            return 1
        fi
        sleep 0.3
    done
    echo "fake releases API is up"
}

# Phases shared by both tests: bring up the fake API, capture the pre-banner
# baseline, wait for the real broadcast, capture the banner, prove the CTA band
# actually changed on screen, and click the CTA where those pixels changed.
run_update_banner_phases() {
    local prefix="$1" point delta

    start_fake_update_api
    resize_window
    shot "/output/${prefix}-00-before-banner.png"
    echo "PHASE 1 PASS: fake releases API up, pre-banner baseline captured"

    # The server's first periodic check fires 30 s after it starts
    # (updater.rs INITIAL_DELAY) and broadcasts UpdateAvailable to every
    # connected window; the client's reader logs it as it lands.
    wait_for_log "$CLIENT_LOG" "update available" 90
    sleep 1.0
    shot "/output/${prefix}-01-update-banner.png"
    echo "PHASE 2 PASS: server broadcast UpdateAvailable and the client took it"

    crop_cta_band "/output/${prefix}-00-before-banner.png" /output/cta-before.png
    crop_cta_band "/output/${prefix}-01-update-banner.png" /output/cta-after.png
    delta=$(cta_band_delta /output/cta-before.png /output/cta-after.png)
    echo "CTA band pixel delta: $delta"
    if [ "${delta:-0}" -lt 40 ]; then
        echo "PHASE 3 FAIL: the centred status-bar band did not change"
        exit 1
    fi
    echo "PHASE 3 PASS: the update CTA rendered into the centred status-bar band"

    point=$(cta_click_point /output/cta-before.png /output/cta-after.png)
    echo "clicking the CTA at $point"
    focus
    # shellcheck disable=SC2086 -- point is an intentional "X Y" pair
    xdotool mousemove $point
    sleep 0.3
    xdotool click 1
    sleep 0.8
    shot "/output/${prefix}-02-update-dialog.png"
    echo "PHASE 4 PASS: clicked the CTA — update confirmation modal captured"
}
