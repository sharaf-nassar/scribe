#!/bin/bash
set -euo pipefail

RESOLUTION="${RESOLUTION:-1920x1080}"
VISUAL_APP="${SCRIBE_VISUAL_APP:-client}"
TEST_TIMEOUT="${TEST_TIMEOUT:-60}"
TEST_HOME="${TEST_HOME:-/tmp/scribe-visual-home}"
DAEMON_STARTED=0
SERVER_STARTED=0

cleanup() {
    kill "${APP_PID:-}" 2>/dev/null || true
    if [ "$DAEMON_STARTED" -eq 1 ]; then
        scribe-test daemon stop >/dev/null 2>&1 || true
    fi
    if [ "$SERVER_STARTED" -eq 1 ]; then
        scribe-test server stop >/dev/null 2>&1 || true
    fi
    kill "${XVFB_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

wait_for_window() {
    local name="$1"
    local timeout_secs="${2:-15}"
    local started
    started=$(date +%s)
    while true; do
        if xdotool search --name "$name" >/dev/null 2>&1; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            echo "Timed out waiting for window: $name" >&2
            return 1
        fi
        sleep 0.2
    done
}

prepare_xdg_dirs() {
    export XDG_CONFIG_HOME="$TEST_HOME/.config"
    export XDG_DATA_HOME="$TEST_HOME/.local/share"
    export XDG_STATE_HOME="$TEST_HOME/.local/state"
    mkdir -p "$XDG_CONFIG_HOME/scribe" "$XDG_DATA_HOME/scribe" "$XDG_STATE_HOME/scribe"
}

Xvfb :99 -screen 0 "${RESOLUTION}x24" &
XVFB_PID=$!
export DISPLAY=:99
sleep 0.5

UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

prepare_xdg_dirs
export PATH="/tests/bin:$PATH"
export RUST_LOG="${RUST_LOG:-scribe_server=info}"

scribe-test server start
SERVER_STARTED=1

case "$VISUAL_APP" in
    client)
        scribe-test daemon start
        DAEMON_STARTED=1
        export WGPU_BACKEND=vulkan
        export LIBGL_ALWAYS_SOFTWARE=1
        scribe-client &
        APP_PID=$!
        wait_for_window "Scribe" 15 || true
        SESSION=$(scribe-test session create)
        export SESSION
        ;;
    *)
        echo "Unsupported SCRIBE_VISUAL_APP value: $VISUAL_APP" >&2
        exit 2
        ;;
esac

EXIT_CODE=0
timeout "$TEST_TIMEOUT" "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
