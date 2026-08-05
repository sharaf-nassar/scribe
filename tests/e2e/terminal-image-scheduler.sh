#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Mandatory Decode Scheduling#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/scheduler.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-scheduler --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "scheduler probe did not write evidence"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "scheduler evidence does not pass"
grep -Fq '"admission": "mandatory"' "$EVIDENCE" \
    || fail "evidence does not claim mandatory admission"
grep -Fq '"production_admitted": 2' "$EVIDENCE" \
    || fail "production Kitty and Sixel decodes did not both take a permit"
grep -Fq '"production_released": 2' "$EVIDENCE" \
    || fail "production decode admissions were not released"
grep -Fq '"released_exactly_once": true' "$EVIDENCE" \
    || fail "a decode admission was released more or less than once"
grep -Fq '"foreign_issuer": "foreign_issuer"' "$EVIDENCE" \
    || fail "a permit from another issuer was authorized"
grep -Fq '"foreign_ticket_issuer": "foreign_issuer"' "$EVIDENCE" \
    || fail "a ticket from another issuer was admitted"
grep -Fq '"foreign_session": "foreign_session"' "$EVIDENCE" \
    || fail "a permit for another session was authorized"
grep -Fq '"foreign_generation": "foreign_generation"' "$EVIDENCE" \
    || fail "a permit for another generation was authorized"
grep -Fq '"foreign_target": "foreign_target"' "$EVIDENCE" \
    || fail "a permit for another target was authorized"
grep -Fq '"foreign_budget": "foreign_budget"' "$EVIDENCE" \
    || fail "a permit for another storage budget was authorized"
grep -Fq '"foreign_budget_bytes": "foreign_budget"' "$EVIDENCE" \
    || fail "a permit for another byte budget was authorized"
grep -Fq '"request_exceeds_ceiling": "request_exceeds_ceiling"' "$EVIDENCE" \
    || fail "an oversized decode request was queued"
grep -Fq '"rejected_before_work": true' "$EVIDENCE" \
    || fail "a refused admission charged storage"
grep -Fq '"barged": false' "$EVIDENCE" || fail "admission did not follow issue order"
grep -Fq '"cancelled_waiter": "cancelled"' "$EVIDENCE" \
    || fail "a cancelled waiter was not retired"
grep -Fq '"successor_admitted": true' "$EVIDENCE" \
    || fail "cancellation did not wake the next waiter"
grep -Fq '"successor_not_cancelled": true' "$EVIDENCE" \
    || fail "cancellation reached an unrelated waiter"
grep -Fq '"in_flight_cancelled": true' "$EVIDENCE" \
    || fail "cancellation did not reach in-flight decode work"
grep -Fq '"in_flight_decode_refused": true' "$EVIDENCE" \
    || fail "a cancelled permit still opened a decode budget"
grep -Fq '"unrelated_target_untouched": true' "$EVIDENCE" \
    || fail "cancellation reached an unrelated target"
grep -Fq '"expired_waiter": "deadline_expired"' "$EVIDENCE" \
    || fail "a queue-wait deadline did not retire its waiter"
grep -Fq '"expired_total": 1' "$EVIDENCE" \
    || fail "deadline retirement was not counted exactly once"
grep -Fq '"successor_admitted_after_release": true' "$EVIDENCE" \
    || fail "deadline retirement blocked the queue"
grep -Fq '"queue_full": "queue_full"' "$EVIDENCE" \
    || fail "the queue depth ceiling was not enforced"
grep -Fq '"queue_depth_ceiling": 2' "$EVIDENCE" \
    || fail "queue depth ceiling drifted"
grep -Fq '"peak_queued": 2' "$EVIDENCE" \
    || fail "queue metadata exceeded its depth ceiling"
grep -Fq '"abandoned_pruned": 2' "$EVIDENCE" \
    || fail "abandoned tickets were not pruned"
grep -Fq '"queued_after_abandon": 0' "$EVIDENCE" \
    || fail "abandoned tickets stayed queued"
grep -Fq '"progressed": true' "$EVIDENCE" \
    || fail "an unrelated session could not progress"
grep -Fq '"session_requested_current": 0' "$EVIDENCE" \
    || fail "session storage was retained after release"
grep -Fq '"process_requested_current": 0' "$EVIDENCE" \
    || fail "process storage was retained after release"

echo "PASS: mandatory decode scheduling evidence at $EVIDENCE"
