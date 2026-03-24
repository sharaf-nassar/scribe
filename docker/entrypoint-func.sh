#!/bin/bash
set -euo pipefail

cleanup() {
    scribe-test daemon stop || true
    scribe-test server stop || true
}
trap cleanup EXIT

UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

# Ensure config directory exists so the file watcher can be initialised.
mkdir -p "${HOME}/.config/scribe"

scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create)
export SESSION

EXIT_CODE=0
timeout 30 "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
