#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: a maximized window comes back maximized.
# @lat: [[test#Visual E2E Tests#A maximized window survives an update]]
#
# The reported failure is an *update*: a maximized window relaunched windowed.
# An update first replaces the server under the live client, then stops the
# client the way `dist/debian/postinst` does — SIGTERM, not the in-app close
# dialog — and relaunches it against the handed-off sessions. That is a
# different restore path from the one a quit takes, and it is the one here.
#
# Both are driven, in order, so a failure names which path broke:
#   0. open two windows, maximize the restored sibling, and prove the record;
#   1. hot-handoff the server, then SIGTERM the client;
#   2. both replacement windows paint with exactly one maximized.
#
# The oracle is `_NET_WM_STATE` read with xprop, not a screenshot: a window that
# merely happens to be screen-sized is not the same as a maximized one, and only
# the EWMH property tells them apart.
#
# Requires: visual container; xdotool, xprop.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
WINDOWS_DIR="$STATE_DIR/windows"

fail() {
    echo "FAIL: $1" >&2
    echo "--- geometry records ---" >&2
    for record in "$WINDOWS_DIR"/*.toml; do
        [ -f "$record" ] || continue
        echo "== $(basename "$record")" >&2
        cat "$record" >&2
    done
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    exit 1
}

scribe_windows() {
    { xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null || true
      xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
    } | sort -u
}

find_window() {
    scribe_windows | tail -1
}

window_count_is() { [ "$(scribe_windows | wc -l)" -eq "$1" ]; }

wait_for_windows() {
    local timeout_secs="$1" started
    shift
    started=$(date +%s)
    until "$@"; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.1
    done
    return 0
}

window_id_is_maximized() {
    local state
    state=$(xprop -id "$1" _NET_WM_STATE 2>/dev/null || true)
    case "$state" in
        *_NET_WM_STATE_MAXIMIZED_VERT*_NET_WM_STATE_MAXIMIZED_HORZ*) return 0 ;;
        *_NET_WM_STATE_MAXIMIZED_HORZ*_NET_WM_STATE_MAXIMIZED_VERT*) return 0 ;;
        *) return 1 ;;
    esac
}

maximized_window_count() {
    local wid total=0
    for wid in $(scribe_windows); do
        if window_id_is_maximized "$wid"; then
            total=$((total + 1))
        fi
    done
    printf '%s' "$total"
}

# Every geometry record's state line, for a failure message that names what was
# actually persisted rather than only what was expected.
dump_records() {
    local record
    for record in "$WINDOWS_DIR"/*.toml; do
        [ -f "$record" ] || continue
        echo "  $(basename "$record" .toml): $(grep -E '^(state|maximized) *=' "$record" | head -1)"
    done
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.8
}

count_log_lines() {
    grep -cF "$2" "$1" 2>/dev/null || true
}

wait_for_log_count() {
    local path="$1" needle="$2" minimum="$3" deadline=$((SECONDS + 20))
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ "$(count_log_lines "$path" "$needle")" -ge "$minimum" ] && return 0
        sleep 0.1
    done
    return 1
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Ask the window manager to maximize the live window.
#
# This is the EWMH `_NET_WM_STATE` client message with `source = application` —
# the exact request a titlebar button or a keyboard shortcut sends, so the
# window manager owns the resulting geometry the same way it would in the
# field. It is sent by hand because neither route the container offers works:
# `xdotool windowstate` only exists from xdotool 3.2020 (this image ships
# 3.2016), `wmctrl` is not installed, and openbox's titlebar-double-click
# binding is a gesture the pointer cannot reliably land on.
maximize_window() {
    local wid="$1"
    [ -n "$wid" ] || fail "no Scribe window to maximize"
    python3 - "$wid" <<'PY'
import ctypes, sys

NET_WM_STATE_ADD = 1
SOURCE_APPLICATION = 1
SUBSTRUCTURE_NOTIFY, SUBSTRUCTURE_REDIRECT = 1 << 19, 1 << 20
CLIENT_MESSAGE = 33

xlib = ctypes.CDLL("libX11.so.6")
xlib.XOpenDisplay.restype = ctypes.c_void_p
xlib.XInternAtom.restype = ctypes.c_ulong
xlib.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
xlib.XDefaultRootWindow.restype = ctypes.c_ulong
xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]


class ClientMessage(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("message_type", ctypes.c_ulong),
        ("format", ctypes.c_int),
        ("data", ctypes.c_long * 5),
        # An XEvent is a union sized to 24 longs; a short send would have the
        # server read past the buffer.
        ("pad", ctypes.c_long * 16),
    ]


display = xlib.XOpenDisplay(None)
if not display:
    sys.exit("could not open the X display")
atom = lambda name: xlib.XInternAtom(display, name.encode(), False)
event = ClientMessage(
    type=CLIENT_MESSAGE,
    window=int(sys.argv[1]),
    message_type=atom("_NET_WM_STATE"),
    format=32,
    data=(ctypes.c_long * 5)(
        NET_WM_STATE_ADD,
        atom("_NET_WM_STATE_MAXIMIZED_VERT"),
        atom("_NET_WM_STATE_MAXIMIZED_HORZ"),
        SOURCE_APPLICATION,
        0,
    ),
)
xlib.XSendEvent(
    ctypes.c_void_p(display),
    ctypes.c_ulong(xlib.XDefaultRootWindow(display)),
    False,
    ctypes.c_long(SUBSTRUCTURE_NOTIFY | SUBSTRUCTURE_REDIRECT),
    ctypes.byref(event),
)
xlib.XFlush(ctypes.c_void_p(display))
PY
    sleep 1.5
}

wait_for_client_exit() {
    local timeout_secs="$1" started
    started=$(date +%s)
    while pgrep -f 'scribe-client' >/dev/null 2>&1; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    return 0
}

launch_client() {
    scribe-client >>"$CLIENT_LOG" 2>&1 &
}

# ── Phase 0: two windows, exactly one maximized on disk ───────────
sleep 1.0
focus
ORIGINAL=$(find_window)
send_keys ctrl+shift+n
wait_for_windows 20 window_count_is 2 \
    || fail "PHASE 0: ctrl+shift+n never opened a second window"
VICTIM=""
for wid in $(scribe_windows); do
    [ "$wid" = "$ORIGINAL" ] || VICTIM="$wid"
done
[ -n "$VICTIM" ] || fail "PHASE 0: could not identify the newly opened sibling"
[ "$(maximized_window_count)" -eq 0 ] \
    || fail "PHASE 0: a window was maximized before the test requested it"
maximize_window "$VICTIM"
window_id_is_maximized "$VICTIM" \
    || fail "PHASE 0: the window manager did not maximize window $VICTIM"
[ "$(maximized_window_count)" -eq 1 ] \
    || fail "PHASE 0: expected exactly one maximized window"
shot /output/00-two-windows-one-maximized.png
# Past the 500ms geometry debounce plus a margin for the flush tick.
sleep 3.0
MAXIMIZED_RECORDS=$(grep -lE '^(state *= *"maximized"|maximized *= *true)' "$WINDOWS_DIR"/*.toml 2>/dev/null | wc -l)
[ "$MAXIMIZED_RECORDS" -eq 1 ] \
    || fail "PHASE 0: expected one maximized record, found $MAXIMIZED_RECORDS
$(dump_records)"
# Retire the harness daemon's off-screen session so `other_windows` contains
# exactly the two UI windows this test created. The daemon stays alive; only its
# unrelated logical window is removed from the upgrade/relaunch population.
SERVER_CLOSES_BEFORE=$(count_log_lines "$SERVER_LOG" "session closed by client")
scribe-test session close "$SESSION"
wait_for_log_count "$SERVER_LOG" "session closed by client" "$((SERVER_CLOSES_BEFORE + 1))" \
    || fail "PHASE 0: harness session was not retired before the handoff"
echo "PHASE 0 PASS: two UI windows persisted with exactly one maximized"

# ── Phase 1: replace the server, then stop the old client ─────────
# This is postinst's order: `spawn_upgrade_server` completes the live fd handoff
# before `stop_client_processes` terminates the captured old client PIDs.
scribe-test server upgrade
kill -0 "$SCRIBE_CLIENT_PID" || fail "PHASE 1: server handoff killed the live client"
[ "$(maximized_window_count)" -eq 1 ] \
    || fail "PHASE 1: server handoff changed the windows' maximized state"
pkill -TERM -x scribe-client || fail "PHASE 1: no scribe-client process to stop"
wait_for_client_exit 20 || fail "PHASE 1: the client survived SIGTERM"
wait_for_windows 10 window_count_is 0 \
    || fail "PHASE 1: an old client window survived process exit"
echo "PHASE 1 PASS: server handed off, then the old client stopped"

# ── Phase 2: both replacements paint with exactly one maximized ──
ASSERTIONS_BEFORE=$(count_log_lines "$CLIENT_LOG" "asserting the restored window's saved state")
REPORTS_BEFORE=$(count_log_lines "$CLIENT_LOG" "reported the workspace tree to the server")
launch_client
wait_for_windows 25 window_count_is 2 \
    || fail "PHASE 2: replacement did not map both restored windows"
REPLACEMENTS=$(scribe_windows)
[ "$(printf '%s\n' "$REPLACEMENTS" | sed '/^$/d' | wc -l)" -eq 2 ] \
    || fail "PHASE 2: replacement XID set was not exactly two: $REPLACEMENTS"
wait_for_log_count "$CLIENT_LOG" "asserting the restored window's saved state" \
    "$((ASSERTIONS_BEFORE + 1))" \
    || fail "PHASE 2: maximized replacement never reached its first restored render"
wait_for_log_count "$CLIENT_LOG" "reported the workspace tree to the server" \
    "$((REPORTS_BEFORE + 2))" \
    || fail "PHASE 2: both replacement windows never reached their restored app frames"
[ "$(maximized_window_count)" -eq 1 ] || fail "PHASE 2: first restored frames did not have \
exactly one maximized window
$(for wid in $REPLACEMENTS; do xprop -id "$wid" _NET_WM_STATE 2>&1; done)
$(dump_records)"
shot /output/01-two-relaunched-one-maximized.png
window_count_is 2 || fail "PHASE 2: replacement window count changed after first render"
[ "$(maximized_window_count)" -eq 1 ] \
    || fail "PHASE 2: a late GPUI toggle changed the one-maximized invariant"
echo "PHASE 2 PASS: both first app frames had exactly one maximized and stayed that way"

echo ""
echo "PASS: visual maximized-restore test"
echo "  Inspect screenshots in test-output/:"
echo "    00-two-windows-one-maximized.png       — before the upgrade"
echo "    01-two-relaunched-one-maximized.png    — both restored app frames"
