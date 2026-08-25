#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Framing Verification]]
set -euo pipefail

exec scribe-test image-framing \
    --fixtures /tests/fixtures/terminal-images \
    --evidence /output/terminal-images/framing.json
