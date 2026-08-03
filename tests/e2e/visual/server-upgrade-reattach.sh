#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#Visual E2E Tests#Server-upgrade reattach oracle]]
# Keep the original GPUI process alive while its server replaces the live
# socket. The shared-pane rig makes SESSION the pane this exact window renders.

count_log_lines() {
    local path="$1"
    local needle="$2"
    grep -cF "$needle" "$path" 2>/dev/null || true
}

wait_for_log_count() {
    local path="$1"
    local needle="$2"
    local minimum="$3"
    local deadline=$((SECONDS + 20))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ "$(count_log_lines "$path" "$needle")" -ge "$minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    echo "Timed out waiting for $minimum occurrences of: $needle" >&2
    return 1
}

scribe-test send "$SESSION" 'echo BEFORE_SERVER_UPGRADE\n'
scribe-test wait-output "$SESSION" "BEFORE_SERVER_UPGRADE"

topologies_before=$(count_log_lines "$SCRIBE_CLIENT_LOG" "rebuilt reconnect topology")
attaches_before=$(count_log_lines "$SCRIBE_CLIENT_LOG" "attaching to session")

# This runs the actual fd handoff; unlike reconnect.sh, it never restarts the
# client process. The old stream closes and the live window must redial.
scribe-test server upgrade

wait_for_log_count "$SCRIBE_CLIENT_LOG" "rebuilt reconnect topology" \
    "$((topologies_before + 1))"
wait_for_log_count "$SCRIBE_CLIENT_LOG" "attaching to session" "$((attaches_before + 1))"
kill -0 "$SCRIBE_CLIENT_PID"

# The harness daemon's own server stream closes during the handoff, so drive the
# process this test is about: the original GPUI window. A substantial body-pixel
# change after typing the sentinel proves its replacement connection accepts
# input and paints the echoed command/output on the retained pane.
wid=$(xdotool search --name "Scribe" | tail -1)
[ -n "$wid" ]
xdotool windowactivate --sync "$wid" 2>/dev/null \
    || xdotool windowfocus --sync "$wid" 2>/dev/null || true
sleep 0.8
eval "$(xdotool getwindowgeometry --shell "$wid")"
scrot -o /output/server-upgrade-before-input.png
convert /output/server-upgrade-before-input.png \
    -crop "${WIDTH}x$(( HEIGHT - 60 ))+${X}+${Y}" +repage \
    /output/server-upgrade-before-input-body.png

xdotool type --clearmodifiers --delay 30 "echo AFTER_SERVER_UPGRADE"
xdotool key --clearmodifiers Return
sleep 1.5
scrot -o /output/server-upgrade-after-input.png
convert /output/server-upgrade-after-input.png \
    -crop "${WIDTH}x$(( HEIGHT - 60 ))+${X}+${Y}" +repage \
    /output/server-upgrade-after-input-body.png
changed=$(compare -metric AE \
    /output/server-upgrade-before-input-body.png \
    /output/server-upgrade-after-input-body.png null: 2>&1 || true)
[ "${changed%% *}" -gt 500 ] \
    || { echo "Post-upgrade terminal body changed only $changed pixels" >&2; exit 1; }

echo "PASS: running client reattached after server upgrade"
