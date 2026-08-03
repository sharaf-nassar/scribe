#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Client Scene#Ordered live fixture stays atomic and bounded]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images/client-scene.json
OUTPUT=/output/terminal-images/client-scene.json

scribe-test terminal-image-client-scene --fixtures "$FIXTURES" --output "$OUTPUT"

for check in \
    atomic_staging \
    ordered_initial_placements \
    replacement_and_grid_effects \
    deletion_frees_definition \
    reset_cleanup \
    partial_and_stale_cleanup \
    placeholder_copy_filtering \
    typed_quota_error \
    mismatch_update_required
do
    grep -Fq "\"$check\": true" "$OUTPUT"
done

echo "PASS: ordered immutable client image scene, cleanup, filtering, and mismatch"
