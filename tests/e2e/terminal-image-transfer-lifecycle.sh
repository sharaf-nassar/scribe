#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Incomplete Transfer Retirement#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/transfer-lifecycle.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-transfer-lifecycle --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "transfer lifecycle probe did not write evidence"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "transfer lifecycle evidence does not pass"
grep -Fq '"engine": "production session terminal seam"' "$EVIDENCE" \
    || fail "evidence did not come from the production seam"
grep -Fq '"partial_apc": "truncated_sequence"' "$EVIDENCE" \
    || fail "a partial APC frame did not retire as a truncated sequence"
grep -Fq '"partial_apc_protocol": "kitty"' "$EVIDENCE" \
    || fail "a partial APC frame lost its protocol"
grep -Fq '"partial_dcs": "truncated_sequence"' "$EVIDENCE" \
    || fail "a partial DCS frame did not retire as a truncated sequence"
grep -Fq '"partial_dcs_protocol": "sixel"' "$EVIDENCE" \
    || fail "a partial DCS frame lost its protocol"
grep -Fq '"split_terminator": "truncated_sequence"' "$EVIDENCE" \
    || fail "a split terminator survived stream end"
grep -Fq '"kitty_chunks": "truncated_sequence"' "$EVIDENCE" \
    || fail "incomplete Kitty chunks were not retired on reset"
grep -Fq '"compressed_chunks": "truncated_sequence"' "$EVIDENCE" \
    || fail "incomplete compressed chunks were not retired on close"
grep -Fq '"candidate_text_retired_as": "raw"' "$EVIDENCE" \
    || fail "stream end did not flush candidate text as raw bytes"
grep -Fq '"published_images": 0' "$EVIDENCE" \
    || fail "an incomplete transfer published an image"
grep -Fq '"definitions": 0' "$EVIDENCE" \
    || fail "an incomplete transfer defined an image"
grep -Fq '"placements": 0' "$EVIDENCE" \
    || fail "an incomplete transfer placed an image"
grep -Fq '"generation_unchanged": true' "$EVIDENCE" \
    || fail "an incomplete transfer consumed a generation"
grep -Fq '"repeated_reset_outputs": 0' "$EVIDENCE" \
    || fail "a repeated reset produced output"
grep -Fq '"repeated_close_outputs": 0' "$EVIDENCE" \
    || fail "a repeated close produced output"
grep -Fq '"counters_stable": true' "$EVIDENCE" \
    || fail "a repeated retirement moved the storage ledger"
grep -Fq '"ledger_healthy": true' "$EVIDENCE" \
    || fail "a repeated retirement underflowed the storage ledger"
grep -Fq '"cancelled": "quota_exceeded"' "$EVIDENCE" \
    || fail "a cancelled admission produced no typed boundary"
grep -Fq '"cancelled_pending_cleared": true' "$EVIDENCE" \
    || fail "a cancelled admission left pending transfer state"
grep -Fq '"cancelled_count": 1' "$EVIDENCE" \
    || fail "cancellation reached the wrong number of entries"
grep -Fq '"deadline": "quota_exceeded"' "$EVIDENCE" \
    || fail "an expired admission produced no typed boundary"
grep -Fq '"deadline_pending_cleared": true' "$EVIDENCE" \
    || fail "an expired admission left pending transfer state"
grep -Fq '"close_cancelled_waiter": true' "$EVIDENCE" \
    || fail "close did not cancel its own queued admission"
grep -Fq '"fifo": true' "$EVIDENCE" \
    || fail "query replies lost FIFO chronology"
grep -Fq '"pending_after_cases": 0' "$EVIDENCE" \
    || fail "a retirement path left pending metadata"
grep -Fq '"retained_bytes_after_cases": 0' "$EVIDENCE" \
    || fail "a retirement path retained image storage"
grep -Fq '"session_requested_current": 0' "$EVIDENCE" \
    || fail "session storage survived retirement"
grep -Fq '"process_requested_current": 0' "$EVIDENCE" \
    || fail "process storage survived retirement"
grep -Fq '"queued": 0' "$EVIDENCE" || fail "a decode waiter survived retirement"
grep -Fq '"active": 0' "$EVIDENCE" || fail "an active decode survived retirement"

echo "PASS: incomplete transfer retirement evidence at $EVIDENCE"
