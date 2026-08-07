#!/bin/bash
set -euo pipefail

export SCRIBE_E2E_SANDBOX=1

RESOLUTION="${RESOLUTION:-1920x1080}"
VISUAL_APP="${SCRIBE_VISUAL_APP:-client}"
# A long corpus declares its own budget with a `# e2e-timeout: <seconds>` line,
# so `just e2e-visual <script>` runs it correctly without a bespoke recipe per
# script. An explicit TEST_TIMEOUT from the caller still wins.
DECLARED_TIMEOUT=$(sed -n 's/^# e2e-timeout: *\([0-9][0-9]*\).*/\1/p' "${1:-/dev/null}" 2>/dev/null | head -1 || true)
TEST_TIMEOUT="${TEST_TIMEOUT:-${DECLARED_TIMEOUT:-60}}"
TEST_HOME="${TEST_HOME:-/tmp/scribe-visual-home}"
DAEMON_STARTED=0
SERVER_STARTED=0

# Opt-in desktop integrations share one disposable session bus. dbus-run-session
# owns its daemon's lifecycle and tears it down after this re-exec exits.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] \
    && { [ "${SCRIBE_KEYRING:-0}" = "1" ] \
        || [ "${SCRIBE_NOTIFY:-0}" = "1" ] \
        || [ "${SCRIBE_IME:-0}" = "1" ] \
        || [ "${SCRIBE_FILE_CHOOSER:-0}" = "1" ]; }; then
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/scribe-visual-runtime-$(id -u)}"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    exec dbus-run-session -- "$0" "$@"
fi

cleanup() {
    kill "${APP_PID:-}" 2>/dev/null || true
    kill "${TAIL_PID:-}" 2>/dev/null || true
    kill "${TAP_PID:-}" 2>/dev/null || true
    kill "${WM_PID:-}" 2>/dev/null || true
    kill "${NOTIFYD_PID:-}" 2>/dev/null || true
    kill "${PORTAL_PID:-}" 2>/dev/null || true
    kill "${PORTAL_GTK_PID:-}" 2>/dev/null || true
    pkill -f ibus-daemon 2>/dev/null || true
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

# Start the GTK directory chooser backend and its public portal frontend.
start_file_chooser_portal() {
    local backend_probe=/output/xdg-desktop-portal-gtk.introspection
    local portal_probe=/output/xdg-desktop-portal.introspection
    local backend_ready=0

    mkdir -p "$XDG_CONFIG_HOME/xdg-desktop-portal"
    printf '[preferred]\ndefault=gtk\n' \
        >"$XDG_CONFIG_HOME/xdg-desktop-portal/portals.conf"

    G_MESSAGES_DEBUG=all /usr/libexec/xdg-desktop-portal-gtk \
        >/output/xdg-desktop-portal-gtk.log 2>&1 &
    PORTAL_GTK_PID=$!
    for _ in {1..50}; do
        if gdbus call --session --dest org.freedesktop.DBus \
            --object-path /org/freedesktop/DBus \
            --method org.freedesktop.DBus.NameHasOwner \
            org.freedesktop.impl.portal.desktop.gtk | grep -q true \
            && gdbus introspect --session \
            --dest org.freedesktop.impl.portal.desktop.gtk \
            --object-path /org/freedesktop/portal/desktop \
            >"$backend_probe" 2>>/output/xdg-desktop-portal-gtk.log \
            && grep -q 'org.freedesktop.impl.portal.FileChooser' "$backend_probe"; then
            backend_ready=1
            break
        fi
        sleep 0.1
    done
    if [ "$backend_ready" -ne 1 ]; then
        echo 'GTK FileChooser portal backend failed to become ready; backend log:' >&2
        tail -n 40 /output/xdg-desktop-portal-gtk.log >&2 || true
        return 1
    fi

    G_MESSAGES_DEBUG=all /usr/libexec/xdg-desktop-portal \
        >/output/xdg-desktop-portal.log 2>&1 &
    PORTAL_PID=$!
    for _ in {1..50}; do
        if gdbus call --session --dest org.freedesktop.DBus \
            --object-path /org/freedesktop/DBus \
            --method org.freedesktop.DBus.NameHasOwner \
            org.freedesktop.portal.Desktop | grep -q true \
            && gdbus introspect --session \
            --dest org.freedesktop.portal.Desktop \
            --object-path /org/freedesktop/portal/desktop \
            >"$portal_probe" 2>>/output/xdg-desktop-portal.log \
            && grep -q 'org.freedesktop.portal.FileChooser' "$portal_probe"; then
            return 0
        fi
        sleep 0.1
    done

    echo 'FileChooser portal failed to become ready; portal logs:' >&2
    tail -n 40 /output/xdg-desktop-portal-gtk.log \
        /output/xdg-desktop-portal.log >&2 || true
    return 1
}

# Seed the feature-014 LAN trust stores before the server starts.
#
# `IpcServerState` loads both TOML stores once at startup, and a single container
# has no second machine to approve and no fingerprintable Wi-Fi to trust — so the
# only way to exercise `RemoveTrustedNetwork` / `RevokeTrustedDevice` against a
# real server is to plant one record of each up front. The documents are the real
# on-disk shape (`version`/`owner` are validated on load), so the server treats
# them exactly like records it wrote itself.
seed_trust_stores() {
    cat >"$XDG_STATE_HOME/scribe/lan_trusted_networks.toml" <<'TOML'
version = 1
owner = "server"
updated_at_ms = 1750000000000

[[networks]]
id = "seeded-network-1"
label = "Seeded Lab Network"
gateway_mac = "aa:bb:cc:dd:ee:01"
subnet_cidr = "10.77.0.0/24"
ssid = "scribe-e2e"
added_at = 1750000000000
TOML
    cat >"$XDG_STATE_HOME/scribe/lan_trusted_devices.toml" <<'TOML'
version = 1
owner = "server"
updated_at_ms = 1750000000000

[[devices]]
device_id = "1f2e3d4c5b6a798877665544332211000f1e2d3c4b5a69788796a5b4c3d2e1f0"
cert_der = "30820122"
label = "seeded-laptop"
first_seen = 1750000000000
TOML
    chmod 600 "$XDG_STATE_HOME/scribe/lan_trusted_networks.toml" \
        "$XDG_STATE_HOME/scribe/lan_trusted_devices.toml"
}

# Start an unlocked gnome-keyring on the harness session bus.
#
# The feature-014 LAN device key is sealed in the OS keyring (Secret Service on
# Linux), and `scribe-server` fails closed without one — so `GetLanDialIdentity`
# reports `available = false` and no mutual-TLS dial can happen at all. Only the
# LAN dial test needs this, so it is opt-in: every other visual test keeps the
# lighter, keyring-free container.
start_session_keyring() {
    # `--unlock` reads the (empty) password on stdin and prints the environment
    # of the daemon it started; only the secrets component is needed.
    eval "$(printf '\n' | gnome-keyring-daemon --unlock --components=secrets)"
    export GNOME_KEYRING_CONTROL
}

# Start a REAL freedesktop notification service on the harness session bus.
#
# The client's dispatcher talks raw zbus to `org.freedesktop.Notifications`; with
# nothing owning that name its `Notify` call fails at the bus and the delivery
# half of the feature leaves no trace at all — indistinguishable from the unwired
# client. `notify-daemon.py` claims the name for real, records every call, and
# can emit `ActionInvoked` on demand, which is what makes click-to-focus
# assertable. Opt-in: every other visual test keeps the lighter container.
start_notification_daemon() {
    export SCRIBE_NOTIFY_RECORD="${SCRIBE_NOTIFY_RECORD:-/output/notifications.jsonl}"
    export SCRIBE_NOTIFY_CONTROL="${SCRIBE_NOTIFY_CONTROL:-/tmp/scribe-notify.ctl}"
    python3 /tests/visual/notify-daemon.py \
        --record "$SCRIBE_NOTIFY_RECORD" \
        --control "$SCRIBE_NOTIFY_CONTROL" >/output/notify-daemon.log 2>&1 &
    NOTIFYD_PID=$!
    for _ in $(seq 1 60); do
        [ -p "$SCRIBE_NOTIFY_CONTROL" ] && [ -f "$SCRIBE_NOTIFY_RECORD" ] && break
        sleep 0.1
    done
}

# Start a real input-method engine so the IME/preedit E2E can compose text.
#
# `--xim` is the load-bearing flag: GPUI's X11 backend talks to an XIM server
# (`X11rbClient` in gpui_linux), and `XMODIFIERS` is how it finds one — without
# both, key presses never reach an input method at all and the raw letters go
# straight to the PTY, which is exactly the unwired symptom this rig has to be
# able to tell apart from a working composition.
#
# The GTK panel is disabled because it wants a tray this container has no use
# for; preedit callbacks and commits are delivered over XIM either way, and the
# client draws its own preedit overlay. `ibus-table-cangjie3` is the engine: a
# table-driven CJK method whose composition is deterministic, so a fixed key
# sequence always produces the same candidate.
start_input_method() {
    export XMODIFIERS='@im=ibus'
    export GTK_IM_MODULE=ibus
    export QT_IM_MODULE=ibus
    export SCRIBE_IME_ENGINE="${SCRIBE_IME_ENGINE:-table:cangjie3}"
    # `--daemonize` is not a convenience: without it ibus-daemon watches the
    # shell that launched it, sees it exit, logs "The parent process died" and
    # takes itself down before any engine can register.
    ibus-daemon --panel disable --xim --replace --daemonize >/output/ibus.log 2>&1
    for _ in $(seq 1 80); do
        if ibus list-engine 2>/dev/null | grep -q "$SCRIBE_IME_ENGINE"; then
            break
        fi
        sleep 0.25
    done
    ibus engine "$SCRIBE_IME_ENGINE" >>/output/ibus.log 2>&1 || true
    echo "ibus engine: $(ibus engine 2>&1)" >>/output/ibus.log
}

# Interpose the recording relay on the server socket. Used by the sharing E2E to
# inject the notices a second machine would have produced, and by the settings
# trust E2E purely for its wire record — the settings window's one-shot server
# actions are transient connections, and the record is the only place their
# frames can be observed leaving the process.
start_share_tap() {
    mv "$UID_DIR/server.sock" "$UID_DIR/server-upstream.sock"
    SHARE_WIRE_RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
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
}

if [ "${SCRIBE_KEYRING:-0}" = "1" ]; then
    start_session_keyring
fi

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
if [ "${SCRIBE_FILE_CHOOSER:-0}" = "1" ]; then
    start_file_chooser_portal
fi
if [ "${SCRIBE_SEED_TRUST:-0}" = "1" ]; then
    seed_trust_stores
fi
export PATH="/tests/bin:$PATH"
export RUST_LOG="${RUST_LOG:-scribe_server=info,scribe_client=info}"

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

# The IME rig needs a UTF-8 locale inside the PTY, not just in the client:
# bash's readline refuses multibyte input in the C locale and rings the bell
# instead of inserting it, so committed CJK characters would never echo. The
# server spawns that shell, so this has to be exported before it starts.
if [ "${SCRIBE_IME:-0}" = "1" ]; then
    export LANG="${LANG:-C.UTF-8}"
    export LC_ALL="${LC_ALL:-C.UTF-8}"
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
            start_share_tap
        fi
        # GPUI renders through blade/Vulkan; the Dockerfile pins
        # VK_ICD_FILENAMES to lavapipe (software) so no GPU is required.
        # LIBGL_ALWAYS_SOFTWARE keeps any GL fallback off hardware too.
        export LIBGL_ALWAYS_SOFTWARE=1
        # Before the client starts: XMODIFIERS is read once, when gpui's X11
        # client builds its XIM connection.
        if [ "${SCRIBE_IME:-0}" = "1" ]; then
            start_input_method
        fi
        # Before the client starts: the dispatcher opens its one session-bus
        # connection at window construction, so the service has to already own
        # the name by then.
        if [ "${SCRIBE_NOTIFY:-0}" = "1" ]; then
            start_notification_daemon
        fi
        export SCRIBE_CLIENT_LOG=/output/client.log
        # Shared-pane rig: create the session FIRST, then start the client as a
        # second participant in the daemon's window.
        #
        # The default order below (client first, session second) leaves each
        # participant on a different pane: the client bootstraps its own shell,
        # while the server sends the daemon's later `SessionCreated` only to the
        # connection that asked for it. Handing the client the daemon's window
        # id closes that gap without evicting the daemon: under
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
        scribe-client >"$SCRIBE_CLIENT_LOG" 2>&1 &
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
            # Most visual scripts drive the daemon's off-window pane. The
            # client has already bootstrapped its own visible login shell; use
            # SCRIBE_SHARED_PANE when both sides must observe the same session.
            SESSION=$(scribe-test session create)
            export SESSION
        fi
        ;;
    settings)
        # The settings window is its own top-level surface (`--settings`,
        # main.rs `run_settings`) and does not register a client connection: every
        # server round-trip it makes is a one-shot transient socket. The tap is
        # therefore mandatory here — it is the only way a script can see which
        # `ClientMessage`s the window actually put on the wire.
        if [ "${SCRIBE_SHARE_TAP:-0}" = "1" ]; then
            start_share_tap
        fi
        export LIBGL_ALWAYS_SOFTWARE=1
        scribe-client --settings >/output/settings.log 2>&1 &
        APP_PID=$!
        export SCRIBE_SETTINGS_PID="$APP_PID"
        export SCRIBE_SETTINGS_LOG=/output/settings.log
        wait_for_window "Scribe Settings" 20 || true
        ;;
    *)
        echo "Unsupported SCRIBE_VISUAL_APP value: $VISUAL_APP" >&2
        exit 2
        ;;
esac

# Run the test with its output on a FILE, never on a pipe.
#
# The test's stdout is inherited by every process it backgrounds. While that
# stdout was `| tee /output/result.log`, a single orphan the script left running
# (a relaunched `scribe-client`, say) held the write end of that pipe open,
# so `tee` never saw EOF and the container hung forever AFTER the test had
# printed PASS. `TEST_TIMEOUT` cannot break the deadlock: it governs the test
# process, which has already exited. Writing to the log and streaming it with a
# `tail` we own makes an orphan harmless — it inherits a file descriptor nothing
# waits on.
: >/output/result.log
tail -n +1 -f /output/result.log &
TAIL_PID=$!

EXIT_CODE=0
timeout "$TEST_TIMEOUT" "$1" >>/output/result.log 2>&1 || EXIT_CODE=$?

# Give tail a moment to flush the final lines, then stop streaming.
sleep 0.5
kill "$TAIL_PID" 2>/dev/null || true
wait "$TAIL_PID" 2>/dev/null || true
TAIL_PID=""

# Reap anything the test left behind so cleanup has nothing to race with.
pkill -f 'scribe-client' 2>/dev/null || true

exit $EXIT_CODE
