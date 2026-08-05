#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Authoritative Image State Assembly#Docker Evidence Entry Point]]
set -euo pipefail

MANIFEST=/output/terminal-images/server-state-manifest.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# Every child gate must already be green in this same output directory, so the
# manifest's assembled claim rests on evidence that passed together.
for gate in state-seam accounting scheduler transfer-lifecycle observer-parity \
    mutations convergence; do
    evidence="/output/terminal-images/${gate}.json"
    [ -s "$evidence" ] || fail "child gate evidence $evidence is missing; run terminal-image-${gate}.sh first"
    grep -Fq '"status": "pass"' "$evidence" || fail "child gate $gate did not pass"
done

scribe-test terminal-image-server-state --evidence "$MANIFEST"

[ -s "$MANIFEST" ] || fail "assembly probe did not write the manifest"
grep -Fq '"schema_version": 1' "$MANIFEST" || fail "manifest version drifted"
grep -Fq '"status": "pass"' "$MANIFEST" || fail "manifest does not pass"
grep -Fq '"engine": "scribe-server authoritative session terminal seam"' "$MANIFEST" \
    || fail "the scenario did not run through the production seam"
grep -Fq '"payload_free": true' "$MANIFEST" || fail "manifest claims retained payloads"

# Cross-invariant scenario coverage.
for case in framing_ordering storage_accounting decode_scheduling observer_effects \
    transactional_mutations incomplete_retirement counter_overflow \
    independent_sessions client_convergence; do
    grep -Fq "\"$case\": \"pass\"" "$MANIFEST" || fail "assembly case $case did not pass"
done

# Typed outcomes rather than bare booleans.
grep -Fq '"incomplete_transfer_retirement": "truncated_sequence"' "$MANIFEST" \
    || fail "an incomplete transfer did not retire with a typed boundary"
grep -Fq '"sequence_overflow": "terminal image output sequence exhausted"' "$MANIFEST" \
    || fail "sequence overflow lacks its typed rejection"
grep -Fq '"generation_overflow": "terminal image generation exhausted"' "$MANIFEST" \
    || fail "generation overflow lacks its typed rejection"
grep -Fq '"session_isolation": "disjoint_state_shared_process"' "$MANIFEST" \
    || fail "independent sessions were not certified"
grep -Fq '"resize_clipping": "both_grids"' "$MANIFEST" \
    || fail "resize clipping evidence is missing"
grep -Fq '"image_eviction": "oldest_first"' "$MANIFEST" \
    || fail "eviction ordering evidence is missing"

# Frozen limits, exact counters, and both convergence hashes.
grep -Fq '"max_images_per_session": 128' "$MANIFEST" || fail "manifest lost the frozen limits"
grep -Fq '"max_process_retained_bytes": 536870912' "$MANIFEST" \
    || fail "manifest lost the process storage ceiling"
grep -Fq '"reserve_before_allocation_calls"' "$MANIFEST" \
    || fail "manifest lost its reserve-before-allocation counter"
grep -Fq '"process_requested_peak"' "$MANIFEST" || fail "manifest lost its process peak"
grep -Fq '"scheduler"' "$MANIFEST" || fail "manifest lost its scheduler counters"
grep -Fq '"converged": true' "$MANIFEST" || fail "a session ended divergent"
[ "$(grep -c '"converged": true' "$MANIFEST")" = "2" ] \
    || fail "both sessions must publish a convergence hash pair"

# Every specification criterion maps to a passing case.
criteria=$(grep -cE '"US[0-9]+\.[0-9]+": \{' "$MANIFEST")
[ "$criteria" = "40" ] || fail "manifest maps $criteria criteria, expected 40"
mapped=$(grep -c '"status": "pass"' "$MANIFEST")
[ "$mapped" -ge 41 ] || fail "a mapped criterion is not marked passing"

# No image payload may reach the manifest.
if grep -qE '"(payload|bytes|data|rgba|pixels)": *\[' "$MANIFEST"; then
    fail "manifest embedded image payload data"
fi

echo "PASS: authoritative image state manifest at $MANIFEST"
