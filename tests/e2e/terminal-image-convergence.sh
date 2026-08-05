#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Client Convergence and Counter Safety#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/convergence.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-convergence --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "convergence probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "convergence evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "convergence evidence does not pass"
grep -Fq '"engine": "scribe-server canonical image publication"' "$EVIDENCE" \
    || fail "probe did not use the production publication engine"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence retained payload data"
grep -Fq '"definitions_and_placements_converge": "pass"' "$EVIDENCE" \
    || fail "definition and placement convergence case missing"
grep -Fq '"removals_converge": "pass"' "$EVIDENCE" || fail "removal convergence case missing"
grep -Fq '"reset_converges": "pass"' "$EVIDENCE" || fail "reset convergence case missing"
grep -Fq '"screen_change_converges": "pass"' "$EVIDENCE" \
    || fail "screen change convergence case missing"
grep -Fq '"scroll_converges": "pass"' "$EVIDENCE" || fail "scroll convergence case missing"
grep -Fq '"resize_converges": "pass"' "$EVIDENCE" || fail "resize convergence case missing"
grep -Fq '"stale_replay_rejected": "pass"' "$EVIDENCE" || fail "stale replay case missing"
grep -Fq '"counter_exhaustion_rejects_before_mutation": "pass"' "$EVIDENCE" \
    || fail "counter exhaustion case missing"

echo "PASS: terminal image client convergence and counter safety"
