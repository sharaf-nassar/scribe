#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Session State Seam#Docker Evidence Entry Point]]
set -euo pipefail

scribe-test terminal-image-state-seam \
    --evidence /output/terminal-images/state-seam.json
exec scribe-test terminal-image-ipc \
    --fixtures /tests/fixtures/terminal-images/ipc.json \
    --output /output/terminal-images/state-seam-ipc.json
