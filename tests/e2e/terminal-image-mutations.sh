#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Transactional Image Mutations#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/mutations.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-mutations --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "mutation probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "mutation evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "mutation evidence does not pass"
grep -Fq '"engine": "scribe-server canonical image mutations"' "$EVIDENCE" \
    || fail "probe did not use the production mutation engine"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence retained payload data"
grep -Fq '"max_images_per_session": 128' "$EVIDENCE" || fail "image ceiling drifted"
grep -Fq '"max_placements_per_session": 1024' "$EVIDENCE" || fail "placement ceiling drifted"
grep -Fq '"atomic_define_and_place": "pass"' "$EVIDENCE" || fail "compound define/place case missing"
grep -Fq '"compound_failure_commits_nothing": "pass"' "$EVIDENCE" \
    || fail "compound rollback case missing"
grep -Fq '"rollback_preserves_prior_state": "pass"' "$EVIDENCE" \
    || fail "storage rollback case missing"
grep -Fq '"exact_delete_identity": "pass"' "$EVIDENCE" || fail "exact delete case missing"
grep -Fq '"omitted_operand_is_not_wildcard": "pass"' "$EVIDENCE" \
    || fail "omitted delete operand case missing"
grep -Fq '"deterministic_image_eviction": "pass"' "$EVIDENCE" || fail "image eviction case missing"
grep -Fq '"deterministic_placement_eviction": "pass"' "$EVIDENCE" \
    || fail "placement eviction case missing"
grep -Fq '"screen_scoped_mutations": "pass"' "$EVIDENCE" || fail "screen scope case missing"
grep -Fq '"kitty_lifecycle_erases": "pass"' "$EVIDENCE" || fail "Kitty lifecycle erase case missing"
grep -Fq '"kitty_immune_to_text_erase": "pass"' "$EVIDENCE" \
    || fail "Kitty text-erase immunity case missing"
grep -Fq '"half_open_area_and_scroll": "pass"' "$EVIDENCE" || fail "half-open bounds case missing"
grep -Fq '"resize_clips_both_grids": "pass"' "$EVIDENCE" || fail "both-grid resize case missing"

echo "PASS: transactional terminal image mutations"
