#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#IPC Contract Verification]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images/ipc.json
OUTPUT=/output/terminal-images/ipc.json

[ -f "$FIXTURES" ] || {
    echo "FAIL: missing terminal-image IPC fixture manifest" >&2
    exit 1
}

scribe-test terminal-image-ipc --fixtures "$FIXTURES" --output "$OUTPUT"

grep -Fq '"old_local_handshake_defaults": true' "$OUTPUT"
grep -Fq '"new_local_handshake_round_trip": true' "$OUTPUT"
grep -Fq '"older_remote_updates_client": true' "$OUTPUT"
grep -Fq '"newer_remote_updates_server": true' "$OUTPUT"
grep -Fq '"max_replay_chunk_bytes": 1048576' "$OUTPUT"

echo "PASS: terminal image IPC bounds, local compatibility, and remote mismatches"
