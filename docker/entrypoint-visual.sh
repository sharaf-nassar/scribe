#!/bin/bash
set -euo pipefail

RESOLUTION=${RESOLUTION:-1920x1080}

cleanup() {
    kill $CLIENT_PID 2>/dev/null || true
    scribe-test daemon stop || true
    scribe-test server stop || true
    kill $XVFB_PID 2>/dev/null || true
}
trap cleanup EXIT

Xvfb :99 -screen 0 "${RESOLUTION}x24" &
XVFB_PID=$!
export DISPLAY=:99
sleep 0.5

UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

scribe-test server start
scribe-test daemon start

export WGPU_BACKEND=gl
scribe-client &
CLIENT_PID=$!

xdotool search --sync --name "scribe" || true

SESSION=$(scribe-test session create)
export SESSION

EXIT_CODE=0
timeout 60 "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
