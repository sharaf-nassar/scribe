#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Bounded Kitty Decoder Verification]]
set -euo pipefail

CONTRACT=/tests/fixtures/terminal-images/contract.json
OUTPUT=/output/terminal-images/kitty-decode-evidence.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test kitty-decode --contract "$CONTRACT" --evidence "$OUTPUT"

[ -s "$OUTPUT" ] || fail "Kitty decoder did not write evidence"
grep -Fq '"all_passed": true' "$OUTPUT" || fail "Kitty evidence does not pass"
grep -Fq '"upstream_revision": "2a3f980245e3ae38b82ade96533e7b450e8477bb"' "$OUTPUT" \
    || fail "PNG upstream revision missing"
grep -Fq '"sha256": "60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61"' "$OUTPUT" \
    || fail "PNG upstream checksum missing"

for case_id in \
    rgb rgba chunked empty_opening_chunk zlib png \
    malformed_base64 chunk_mismatch raw_length_mismatch \
    non_png_format truncated_png indirect_sources allocation_failure \
    deadline cancellation zlib_bomb
do
    grep -Fq "\"id\": \"$case_id\"" "$OUTPUT" || fail "evidence omits $case_id"
done

grep -Fq '"default_rejection": "work_budget_exceeded"' "$OUTPUT" \
    || fail "default zlib bomb did not hit work budget first"
grep -Fq '"isolated_rejection": "quota_exceeded"' "$OUTPUT" \
    || fail "isolated zlib bomb did not hit inflated-byte quota"
grep -Fq '"attempted": 67108865' "$OUTPUT" \
    || fail "zlib bomb max-plus-one boundary drifted"

echo "PASS: bounded Kitty decoder evidence written to $OUTPUT"
