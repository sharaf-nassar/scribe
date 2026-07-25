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

wait_for_log() {
    local pattern="$1"
    local timeout_secs="${2:-15}"
    local started
    started=$(date +%s)
    while true; do
        if grep -qF "$pattern" "$SCRIBE_CLIENT_LOG" 2>/dev/null; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            echo "Timed out waiting for client log line: $pattern" >&2
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
# Exported so a test can address the server socket directly — that path is also
# the AI hook channel's endpoint (`SCRIBE_HOOK_SOCK`), which is how a scripted
# provider event reaches the server without going through the test daemon.
export SCRIBE_RUNTIME_DIR="$UID_DIR"

prepare_xdg_dirs
export PATH="/tests/bin:$PATH"
export RUST_LOG="${RUST_LOG:-scribe_server=info,scribe_client_gpui=info}"

# Persist the server's tracing output for the same reason the client's is
# persisted below: some behaviour under test leaves no pixels behind and has no
# client-side proxy (e.g. "the server received TriggerUpdate and started an
# install"), so the script asserts on the server log instead of guessing.
export SCRIBE_TEST_SERVER_LOG=/output/server.log
export SCRIBE_SERVER_LOG=/output/server.log
: >"$SCRIBE_SERVER_LOG"

# Config is assembled BEFORE the server starts, because both processes read the
# same file and only one of them re-reads it: the client picks its keys up at
# launch (and again through its watcher), while `[remote].sharing_mode` is read
# by the server's remote supervisor on startup.
#
#   SCRIBE_EXTRA_CONFIG — opt-in client settings for a single test
#                         (e.g. terminal.paste_confirmation).
#   SCRIBE_SHARED_PANE  — the shared-pane rig (see below). Appended last so its
#                         `[remote]` header can never swallow keys that belong
#                         to a table SCRIBE_EXTRA_CONFIG opened.
CONFIG_FILE="$XDG_CONFIG_HOME/scribe/config.toml"
if [ -n "${SCRIBE_EXTRA_CONFIG:-}" ]; then
    printf '%s\n' "$SCRIBE_EXTRA_CONFIG" > "$CONFIG_FILE"
fi
if [ "${SCRIBE_SHARED_PANE:-0}" = "1" ]; then
    printf '\n[remote]\nsharing_mode = "free_for_all"\n' >> "$CONFIG_FILE"
fi

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
        # GPUI renders through blade/Vulkan; the Dockerfile pins
        # VK_ICD_FILENAMES to lavapipe (software) so no GPU is required.
        # LIBGL_ALWAYS_SOFTWARE keeps any GL fallback off hardware too.
        export LIBGL_ALWAYS_SOFTWARE=1
        export SCRIBE_CLIENT_LOG=/output/client.log
        # Shared-pane rig: create the session FIRST, then start the client as a
        # second participant in the daemon's window.
        #
        # The default order below (client first, session second) leaves the
        # client blind: the server sends `SessionCreated` only to the connection
        # that asked for it, and `ListSessions` answers a window with its OWN
        # sessions, so a client that got its own window renders an empty grid
        # while the daemon's pane runs untouched. Handing the client the
        # daemon's window id closes that gap without evicting the daemon: under
        # `sharing_mode = "free_for_all"` the server resolves a non-takeover
        # claim of a connected window as an ADDITIVE join, so both processes
        # stay attached to the same pane — the client renders and types into it,
        # and `scribe-test send` / `wait-output` / `snapshot` keep working
        # against the very pane on screen.
        if [ "${SCRIBE_SHARED_PANE:-0}" = "1" ]; then
            SESSION=$(scribe-test session create)
            export SESSION
            SCRIBE_JOIN_WINDOW=$(scribe-test daemon window-id)
            export SCRIBE_JOIN_WINDOW
        fi
        # Persist the client's tracing output so scripted tests can assert on
        # runtime behaviour that leaves no pixels behind (e.g. the config
        # watcher's "config hot-reloaded" line) instead of guessing from a
        # screenshot diff.
        scribe-client-gpui >"$SCRIBE_CLIENT_LOG" 2>&1 &
        APP_PID=$!
        export SCRIBE_CLIENT_PID="$APP_PID"
        wait_for_window "Scribe" 15 || true
        if [ "${SCRIBE_SHARED_PANE:-0}" = "1" ]; then
            # Attaching is a full Hello / ListSessions / AttachSessions /
            # SessionReplay round trip. Gate on the client's own "attaching to
            # session" line so the test body never drives a window that is still
            # an empty grid — a failure mode a screenshot cannot distinguish
            # from an idle pane.
            wait_for_log "attaching to session" 20 \
                || echo "WARNING: client never logged an attach" >&2
        else
            SESSION=$(scribe-test session create)
            export SESSION
        fi
        ;;
    *)
        echo "Unsupported SCRIBE_VISUAL_APP value: $VISUAL_APP" >&2
        exit 2
        ;;
esac

EXIT_CODE=0
timeout "$TEST_TIMEOUT" "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
