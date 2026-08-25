#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Bounded Kitty Decoder Verification]]
set -euo pipefail

exec scribe-test kitty-decode \
    --contract /tests/fixtures/terminal-images/contract.json \
    --evidence /output/terminal-images/kitty-decode-evidence.json
