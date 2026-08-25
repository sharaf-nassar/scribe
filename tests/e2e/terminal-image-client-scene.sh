#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Client Scene#Ordered live fixture stays atomic and bounded]]
set -euo pipefail

exec scribe-test terminal-image-client-scene \
    --fixtures /tests/fixtures/terminal-images/client-scene.json \
    --output /output/terminal-images/client-scene.json
