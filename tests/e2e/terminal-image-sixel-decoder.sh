#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Bounded Sixel Decoder Verification]]
set -euo pipefail

CONTRACT=/tests/fixtures/terminal-images/contract.json
FIXTURES=/tests/fixtures/terminal-images
OUTPUT=/output/terminal-images/sixel-decoder-evidence.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test sixel-decoder \
    --contract "$CONTRACT" \
    --fixtures "$FIXTURES" \
    --evidence "$OUTPUT"

[ -s "$OUTPUT" ] || fail "Sixel decoder did not write evidence"
grep -Fq '"all_passed": true' "$OUTPUT" || fail "Sixel decoder evidence does not pass"
grep -Fq '"revision": "998cbb2c6d8ed5272f9cc4702a4660778972bf3f"' "$OUTPUT" \
    || fail "upstream revision missing from evidence"
grep -Fq '"sha256": "85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5"' "$OUTPUT" \
    || fail "upstream checksum missing from evidence"

for case_id in \
    owned_7bit_fixture \
    owned_c1_transparent_fixture \
    background_modes \
    palette_repeat \
    raster_attributes \
    dimensions_max \
    dimensions_max_plus_one \
    repeat_growth_max \
    repeat_growth_max_plus_one \
    allocation_failure \
    cancellation_immediate \
    cancellation_cooperative \
    deadline \
    malformed_truncated \
    numeric_overflow \
    palette_max \
    palette_max_plus_one \
    work_max \
    work_max_plus_one
do
    grep -Fq "\"id\": \"$case_id\"" "$OUTPUT" || fail "evidence omits $case_id"
done

echo "PASS: bounded vendored Sixel decoder evidence written to $OUTPUT"
