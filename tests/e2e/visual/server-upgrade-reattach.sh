#!/bin/bash
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
xdotool search --name "Scribe" >/dev/null

# A post-handoff byte reaches the same session only after the replacement
# connection's Hello/ListSessions/AttachSessions flow has completed.
scribe-test send "$SESSION" 'echo AFTER_SERVER_UPGRADE\n'
scribe-test wait-output "$SESSION" "AFTER_SERVER_UPGRADE"

echo "PASS: running client reattached after server upgrade"
