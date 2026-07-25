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
    kill "${TAP_PID:-}" 2>/dev/null || true
    kill "${WM_PID:-}" 2>/dev/null || true
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

# A window manager must own _NET_ACTIVE_WINDOW: the GPUI client's active-window
# guard (mirroring crates/scribe-client/src/x11_focus.rs) suppresses synthetic
# key input whenever that root property does not name our window, and only a WM
# sets it under Xvfb. openbox also gives xdotool a real window to focus and
# raise before scrot captures the frame.
openbox &
WM_PID=$!
sleep 0.6

UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

prepare_xdg_dirs
export PATH="/tests/bin:$PATH"
export RUST_LOG="${RUST_LOG:-scribe_server=info,scribe_client_gpui=info}"

scribe-test server start
SERVER_STARTED=1

case "$VISUAL_APP" in
    client)
        scribe-test daemon start
        DAEMON_STARTED=1
        # Feature 015 sharing needs a second machine, which a single container
        # cannot supply. SCRIBE_SHARE_TAP=1 instead interposes a transparent
        # relay on the server socket: the client still handshakes with the real
        # server over the real framed protocol, every frame is recorded for
        # on-the-wire assertions, and the four share notices a remote peer would
        # have caused are injected through the tap's control socket. The daemon
        # has already connected directly, so the client is the tap's newest
        # connection and therefore the injection target.
        if [ "${SCRIBE_SHARE_TAP:-0}" = "1" ]; then
            mv "$UID_DIR/server.sock" "$UID_DIR/server-upstream.sock"
            SHARE_WIRE_RECORD=/output/share-wire.jsonl
            SHARE_TAP_CONTROL="$UID_DIR/share-tap.sock"
            export SHARE_WIRE_RECORD SHARE_TAP_CONTROL
            scribe-test share-tap \
                --listen "$UID_DIR/server.sock" \
                --upstream "$UID_DIR/server-upstream.sock" \
                --record "$SHARE_WIRE_RECORD" \
                --control "$SHARE_TAP_CONTROL" >/output/share-tap.log 2>&1 &
            TAP_PID=$!
            for _ in $(seq 1 50); do
                [ -S "$SHARE_TAP_CONTROL" ] && break
                sleep 0.1
            done
        fi
        # Optional: seed a config.toml before the client starts so tests can
        # exercise opt-in settings (e.g. terminal.paste_confirmation). No-op
        # when unset, so existing visual tests are unaffected.
        if [ -n "${SCRIBE_EXTRA_CONFIG:-}" ]; then
            printf '%s\n' "$SCRIBE_EXTRA_CONFIG" > "$XDG_CONFIG_HOME/scribe/config.toml"
        fi
        # GPUI renders through blade/Vulkan; the Dockerfile pins
        # VK_ICD_FILENAMES to lavapipe (software) so no GPU is required.
        # LIBGL_ALWAYS_SOFTWARE keeps any GL fallback off hardware too.
        export LIBGL_ALWAYS_SOFTWARE=1
        # Persist the client's tracing output so scripted tests can assert on
        # runtime behaviour that leaves no pixels behind (e.g. the config
        # watcher's "config hot-reloaded" line) instead of guessing from a
        # screenshot diff.
        scribe-client-gpui >/output/client.log 2>&1 &
        APP_PID=$!
        export SCRIBE_CLIENT_PID="$APP_PID"
        export SCRIBE_CLIENT_LOG=/output/client.log
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
