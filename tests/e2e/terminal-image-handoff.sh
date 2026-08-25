#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Handoff#Docker Evidence Entry Point]]
set -euo pipefail

exec scribe-test terminal-image-handoff \
    --fixtures /tests/fixtures/terminal-images \
    --evidence /output/terminal-images/handoff.json
