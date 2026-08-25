#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Authoritative Image State Assembly#Docker Evidence Entry Point]]
set -euo pipefail

exec scribe-test terminal-image-server-state \
    --evidence /output/terminal-images/server-state-manifest.json
