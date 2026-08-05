#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Staged Client Image Replay#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/client-replay.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-client-replay --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "client replay probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "client replay evidence does not pass"
grep -Fq '"engine": "scribe-client staged terminal image replay"' "$EVIDENCE" \
    || fail "the probe did not run through the production client staging seam"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence claims retained payloads"

for case in no_partial_scene ordered_post_commit_live stale_generation_never_resurrects \
    corrupt_replay_recovers staged_state_cleaned_up; do
    grep -Fq "\"$case\": \"pass\"" "$EVIDENCE" || fail "case $case did not pass"
done

# The whole snapshot stages off-screen: only the commit changes what is
# published, and no record before it leaves image state visible.
grep -Fq '"published_identity_changes": 1' "$EVIDENCE" \
    || fail "the snapshot published more than once"
grep -Fq '"partial_observations": 0' "$EVIDENCE" \
    || fail "a staged record leaked into the published scene"
grep -Fq '"replay_records": 11' "$EVIDENCE" || fail "the planned burst changed shape"
grep -Fq '"staged_records": 10' "$EVIDENCE" || fail "a record other than the commit published"
grep -Fq '"committed_definitions": 3' "$EVIDENCE" || fail "the published scene lost definitions"
grep -Fq '"canonical_definitions": 3' "$EVIDENCE" || fail "the canonical scene changed"
grep -Fq '"committed_placements": 3' "$EVIDENCE" || fail "the published scene lost placements"
grep -Fq '"canonical_placements": 3' "$EVIDENCE" || fail "the canonical placements changed"

# Live records that arrive mid-snapshot are held back and then applied in
# arrival order, reaching the scene an unbuffered stream would have produced.
grep -Fq '"applied_before_commit": 0' "$EVIDENCE" \
    || fail "a buffered live record changed the published scene"
grep -Fq '"buffered_live_records": 10' "$EVIDENCE" || fail "the live buffer did not hold the burst"
grep -Fq '"matches_direct_live_order": true' "$EVIDENCE" \
    || fail "draining the buffer changed the live application order"
grep -Fq '"through_sequence_advanced": true' "$EVIDENCE" \
    || fail "the drained records did not advance the output cursor"

# A superseded generation cannot come back as a snapshot or as buffered deltas.
grep -Fq '"stale_snapshot_rejected": "stale_generation"' "$EVIDENCE" \
    || fail "an older-generation snapshot was not typed-rejected"
grep -Fq '"published_scene_preserved": true' "$EVIDENCE" \
    || fail "the refused snapshot still replaced the published scene"
grep -Fq '"stale_buffered_records": 10' "$EVIDENCE" \
    || fail "the stale live stream was not buffered"
grep -Fq '"resurrected_definitions": 0' "$EVIDENCE" \
    || fail "the drain resurrected a superseded definition"
grep -Fq '"resurrected_placements": 0' "$EVIDENCE" \
    || fail "the drain resurrected a superseded placement"
grep -Fq '"definitions_after_drain": 1' "$EVIDENCE" \
    || fail "the drain changed the snapshot's own definitions"

# Every corrupt burst is typed, leaves the published scene alone, and clears
# its staged state; one clean burst afterwards recovers the pane.
for corruption in record_without_begin commit_without_begin placement_before_definition \
    dropped_definition_chunk truncated_burst foreign_generation_record; do
    grep -Fq "\"$corruption\": \"terminal image" "$EVIDENCE" \
        || fail "corruption $corruption produced no typed refusal"
done
grep -Fq '"scene_preserved_across_failures": "pass"' "$EVIDENCE" \
    || fail "a corrupt burst changed the published scene"
grep -Fq '"staging_cleared_after_failure": "pass"' "$EVIDENCE" \
    || fail "a corrupt burst left staged state behind"
grep -Fq '"fresh_replay_recovered": "pass"' "$EVIDENCE" \
    || fail "the recovery burst published nothing"
grep -Fq '"recovered_definitions": 3' "$EVIDENCE" || fail "the recovery burst lost definitions"

# An abandoned snapshot and a superseded scene both release what they held.
grep -Fq '"superseded_pixels_released": true' "$EVIDENCE" \
    || fail "a superseded scene still owns its canonical pixels"
grep -Fq '"live_buffer_ceiling": 4096' "$EVIDENCE" || fail "the live buffer ceiling drifted"
grep -Fq '"buffer_overflow_error": "live_buffer_overflow"' "$EVIDENCE" \
    || fail "the live buffer did not refuse past its ceiling"
grep -Fq '"buffer_overflow_aborted_snapshot": true' "$EVIDENCE" \
    || fail "buffer overflow kept its staged snapshot"
grep -Fq '"buffered_after_overflow": 0' "$EVIDENCE" \
    || fail "buffer overflow left buffered records behind"

# No image payload may reach the evidence.
if grep -qE '"(payload|bytes|data|rgba|pixels)": *\[' "$EVIDENCE"; then
    fail "evidence embedded image payload data"
fi

echo "PASS: staged client terminal image replay at $EVIDENCE"
