#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#Test Harness#AI Hook Helper#Packaged helper waits for server close]]
# Exercise the staged release binary against a minimal Unix-socket peer. The
# peer reads the complete length-prefixed frame but deliberately stays open;
# the helper must stay alive until that peer closes, which keeps macOS
# getpeereid valid between accept and dispatch.
SOCKET=/tmp/scribe-hook-helper-lifetime.sock
RESULT=/tmp/scribe-hook-helper-lifetime-result
rm -f "$SOCKET" "$RESULT"

SERVER_PID=""
HELPER_PID=""
cleanup() {
    [ -z "$HELPER_PID" ] || kill "$HELPER_PID" 2>/dev/null || true
    [ -z "$SERVER_PID" ] || kill "$SERVER_PID" 2>/dev/null || true
    rm -f "$SOCKET" "$RESULT"
}
trap cleanup EXIT

python3 - "$SOCKET" "$RESULT" <<'PY' &
import socket
import struct
import sys
import time

socket_path, result_path = sys.argv[1:]
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(socket_path)
server.listen(1)
connection, _ = server.accept()

prefix = connection.recv(4)
if len(prefix) != 4:
    raise SystemExit("helper closed before the frame prefix arrived")
remaining = struct.unpack(">I", prefix)[0]
while remaining:
    chunk = connection.recv(remaining)
    if not chunk:
        raise SystemExit("helper closed before the complete frame arrived")
    remaining -= len(chunk)

# Give the old write-and-exit implementation time to send FIN, then inspect the
# connection without consuming anything. EAGAIN means the peer is alive and
# waiting; b"" means it already closed and recreated the macOS race.
time.sleep(0.03)
connection.setblocking(False)
try:
    pending = connection.recv(1, socket.MSG_PEEK)
except BlockingIOError:
    pending = None
if pending is not None:
    raise SystemExit("helper closed before the server closed its connection")

open(result_path, "w").write("connected\n")
connection.close()
server.close()
PY
SERVER_PID=$!

for _ in $(seq 1 100); do
    [ -S "$SOCKET" ] && break
    sleep 0.01
done
[ -S "$SOCKET" ] || { echo "FAIL: stub server did not bind"; exit 1; }

SCRIBE_HOOK_SOCK="$SOCKET" \
SCRIBE_SESSION_ID=3b0608ee-7569-4c0d-a79d-172a2630b35a \
scribe-hook-helper --provider=claude_code --event=state_changed --state=processing &
HELPER_PID=$!

if ! wait "$SERVER_PID"; then
    SERVER_PID=""
    wait "$HELPER_PID" || true
    HELPER_PID=""
    echo "FAIL: stub server observed an early helper disconnect"
    exit 1
fi
SERVER_PID=""
wait "$HELPER_PID"
HELPER_PID=""
[ "$(cat "$RESULT")" = "connected" ] || { echo "FAIL: missing lifetime result"; exit 1; }

echo "PASS: packaged hook helper remains connected until server close"
