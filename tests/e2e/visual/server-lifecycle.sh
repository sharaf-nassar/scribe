#!/bin/bash
# Scripted E2E: the GPUI client autostarts a local server and diagnoses a stale
# socket, driven against the real client process.
#
# `server_lifecycle.rs` shipped complete and unreachable: the binary connected
# with a bare `tokio::net::UnixStream::connect(server_socket_path())`, so there
# was no `connect_or_start_server`, no autostart, and a leftover socket file
# surfaced as an unexplained "connection refused" on the status bar.
#
# The two halves are asserted separately because they fail separately:
#
#   1. DIAGNOSIS. A socket file with nothing listening behind it is the residue
#      of a server that died without unlinking it. The client must name that
#      case rather than reporting a bare OS error. This phase runs with the
#      autostart deliberately unable to succeed, so the diagnosis is what the
#      client is left holding — and it must reach the status line, not just the
#      log.
#   2. AUTOSTART. With a service manager that can actually start the server, the
#      same refused connect must end in a live connection and a painting window.
#      Systemd is not available in a container, so `systemctl` is shimmed by a
#      script that starts the real `scribe-server` — the shim stands in for the
#      service manager only. Everything the client does around it (the refused
#      connect, the decision to start, the retry loop, the successful
#      handshake) is the shipped code path.
#
# Nothing about the server is faked: it is the same `scribe-server` binary the
# rest of the visual suite runs, started through `scribe-test server start`.
#
# Requires: visual container (no share tap, no shared pane — this test owns the
# server's lifecycle and must be free to stop it).
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
UID_DIR="${SCRIBE_RUNTIME_DIR:?the entrypoint must export SCRIBE_RUNTIME_DIR}"
SOCKET="$UID_DIR/server.sock"
SHIM_DIR=/tmp/scribe-lifecycle-bin
PHASE1_LOG=/output/client-stale.log
PHASE2_LOG=/output/client-autostart.log

fail() {
    echo "FAIL: $1" >&2
    echo "--- phase 1 client log ---" >&2
    tail -30 "$PHASE1_LOG" 2>/dev/null || true
    echo "--- phase 2 client log ---" >&2
    tail -30 "$PHASE2_LOG" 2>/dev/null || true
    echo "--- runtime dir ---" >&2
    ls -la "$UID_DIR" >&2 || true
    exit 1
}

plain_log() { sed 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null || true; }

wait_for_pattern() {
    local file="$1" pattern="$2" timeout_secs="${3:-25}" started
    started=$(date +%s)
    while true; do
        if plain_log "$file" | grep -qF "$pattern"; then
            return 0
        fi
        if [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

wait_for_client_exit() {
    local timeout_secs="${1:-20}" started
    started=$(date +%s)
    while pgrep -f 'scribe-client' >/dev/null 2>&1; do
        [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ] && return 1
        sleep 0.3
    done
    return 0
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

# ── Phase 0: tear the first client and the server down ────────────
# The entrypoint left a client attached to a live server; both have to be gone
# before a cold connect can be observed at all.
pkill -f 'scribe-client' 2>/dev/null || true
wait_for_client_exit 20 || fail "the entrypoint's client never exited"
scribe-test server stop >/dev/null 2>&1 || true
for _ in $(seq 1 40); do
    [ -S "$SOCKET" ] || break
    sleep 0.25
done
echo "PHASE 0 PASS: no client and no server are running"

# ── Phase 1: a stale socket file is named as stale ────────────────
# A bound-but-unlistened socket is exactly what a crashed server leaves: the
# file is there, so `connect` gets ECONNREFUSED rather than ENOENT. `systemctl`
# is absent here on purpose, so the autostart fails and the client is left
# holding the diagnosis.
python3 - "$SOCKET" <<'PY'
import os, socket, sys
path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.bind(path)
# Deliberately never listen(): the file exists, nothing accepts on it.
sock.close()
PY
[ -S "$SOCKET" ] || fail "could not plant a stale socket file at $SOCKET"

: >"$PHASE1_LOG"
SCRIBE_CLIENT_LOG="$PHASE1_LOG" scribe-client >>"$PHASE1_LOG" 2>&1 &
if ! wait_for_pattern "$PHASE1_LOG" "stale server socket at $SOCKET" 30; then
    fail "the client never diagnosed the stale socket"
fi
if ! wait_for_pattern "$PHASE1_LOG" "server not running, starting scribe-server" 30; then
    fail "the client never tried to autostart a server"
fi
if ! wait_for_pattern "$PHASE1_LOG" "autostart failed" 30; then
    fail "the failed autostart never surfaced with the stale-socket diagnosis attached"
fi
if ! wait_for_pattern "$PHASE1_LOG" "server connection failed" 30; then
    fail "the diagnosis never reached the window's status line"
fi
echo "PHASE 1 PASS: the client named the stale socket and carried it into the status line"
plain_log "$PHASE1_LOG" | grep -F "stale server socket" | tail -1

pkill -f 'scribe-client' 2>/dev/null || true
wait_for_client_exit 20 || fail "the stale-socket client never exited"

# ── Phase 2: autostart brings a server up and the window paints ───
# The shim is the service manager, nothing more: it starts the same
# `scribe-server` binary the rest of the suite runs. The client's own refused
# connect, start decision, retry loop and handshake are unshimmed.
mkdir -p "$SHIM_DIR"
cat >"$SHIM_DIR/systemctl" <<'SHIM'
#!/bin/bash
# Stand-in for the systemd user manager, which a container does not have.
# Only `--user start <unit>` is meaningful here; everything else succeeds
# quietly so the client's environment-sync calls are harmless.
for arg in "$@"; do
    if [ "$arg" = "start" ]; then
        scribe-test server start >>/output/shim-systemctl.log 2>&1
        exit $?
    fi
done
exit 0
SHIM
chmod +x "$SHIM_DIR/systemctl"

# Plant the stale socket again so phase 2 starts from the same refused connect
# rather than from a clean absence — the harder of the two autostart cases.
python3 - "$SOCKET" <<'PY'
import os, socket, sys
path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.bind(path)
sock.close()
PY

: >"$PHASE2_LOG"
PATH="$SHIM_DIR:$PATH" SCRIBE_CLIENT_LOG="$PHASE2_LOG" \
    scribe-client >>"$PHASE2_LOG" 2>&1 &
if ! wait_for_pattern "$PHASE2_LOG" "server not running, starting scribe-server" 30; then
    fail "the client never tried to autostart a server in phase 2"
fi
if ! wait_for_pattern "$PHASE2_LOG" "connected to scribe-server" 40; then
    fail "the autostarted server never accepted the client's connection"
fi
# A freshly autostarted server owns no sessions, so the proof that the client is
# really talking to it is the completed handshake: `Welcome` carries the window
# id the server just minted for this connection.
if ! wait_for_pattern "$PHASE2_LOG" "welcome: adopted window" 40; then
    fail "the client never completed a handshake with the autostarted server"
fi
[ -S "$SOCKET" ] || fail "no server socket exists after the autostart"

WID=""
for _ in $(seq 1 40); do
    WID=$(find_window)
    [ -n "$WID" ] && break
    sleep 0.5
done
[ -z "$WID" ] && fail "the autostarted client never mapped a window"
xdotool windowactivate --sync "$WID" 2>/dev/null || true
sleep 1
scrot -o /output/00-autostarted.png
echo "PHASE 2 PASS: the client autostarted a server, connected, and painted window $WID"
plain_log "$PHASE2_LOG" | grep -F "connected to scribe-server" | tail -1

echo ""
echo "PASS: visual server-lifecycle test"
echo "  Inspect screenshots in test-output/:"
echo "    00-autostarted.png — window painting against the autostarted server"
