#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Framing Verification]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images
EVIDENCE=/output/terminal-images/framing.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test image-framing --fixtures "$FIXTURES" --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "framing probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "evidence schema drifted"
grep -Fq '"all_passed": true' "$EVIDENCE" || fail "evidence does not pass"
grep -Fq '"owned_fixture_count": 10' "$EVIDENCE" || fail "fixture count drifted"
grep -Fq '"payload_bytes_recorded": false' "$EVIDENCE" \
    || fail "evidence payload policy drifted"

for case_id in \
    owned_fixture_split_invariance \
    seven_bit_and_c1_framing \
    can_sub_recovery \
    malformed_and_unsupported \
    overlap_safe_termination \
    candidate_control_resynchronization \
    sixel_parameter_field_bounds \
    truncated_sequence \
    control_string_exact_and_over_budget \
    sixel_header_max_plus_one_discard \
    cancellation_preserves_first_failure \
    kitty_chunk_4096_and_max_plus_one \
    sixel_mode_parsing \
    raw_range_tiling \
    adjacent_text_preservation
do
    grep -Fq "\"id\": \"$case_id\"" "$EVIDENCE" \
        || fail "evidence omits $case_id"
done

echo "PASS: framing evidence written to $EVIDENCE"
