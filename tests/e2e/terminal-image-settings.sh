#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Image Settings and Diagnostics#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/settings.json
RUN_LOG=/output/terminal-images/settings-run.log
CONFIG_ROOT="$(mktemp -d)"
trap 'rm -rf "$CONFIG_ROOT"' EXIT

mkdir -p "$(dirname "$EVIDENCE")"
set +e
XDG_CONFIG_HOME="$CONFIG_ROOT" scribe-test terminal-image-settings \
    --fixtures /tests/fixtures/terminal-images \
    --evidence "$EVIDENCE" >"$RUN_LOG" 2>&1
status=$?
set -e
if [ "$status" -ne 0 ]; then
    cat "$RUN_LOG" >&2
    exit "$status"
fi

if grep -Fq '/wAA' "$RUN_LOG" || grep -q $'\033_G' "$RUN_LOG"; then
    echo "FAIL: settings probe log leaked image payload data" >&2
    exit 1
fi

echo "PASS: terminal image settings and diagnostics at $EVIDENCE"
