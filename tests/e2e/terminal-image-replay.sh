#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Combined Image Replay#Docker Evidence Entry Point]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images
EVIDENCE=/output/terminal-images/replay.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-replay --fixtures "$FIXTURES" --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "replay probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "replay evidence does not pass"
grep -Fq '"engine": "scribe-server combined image replay and backpressure recovery"' "$EVIDENCE" \
    || fail "the probe did not run through the production replay seam"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence claims retained payloads"

# Every acceptance behavior maps to a named case.
for case in bounded_maximum_scene_chunks atomic_late_attach live_buffered_behind_replay \
    dropped_output_recovery viewerless_output simultaneous_viewers \
    no_per_sink_retained_duplicate_scene; do
    grep -Fq "\"$case\": \"pass\"" "$EVIDENCE" || fail "case $case did not pass"
done

# The maximum scene v1 admits travels as wire-sized chunks: 128 MiB of canonical
# RGBA across two 4096x4096 images, split into exactly 128 one-MiB chunks, and
# no single record anywhere near the IPC message ceiling.
grep -Fq '"definitions": 2' "$EVIDENCE" || fail "the maximum scene lost images"
grep -Fq '"total_rgba_bytes": 134217728' "$EVIDENCE" \
    || fail "the maximum scene is not the frozen session retention ceiling"
grep -Fq '"chunks": 128' "$EVIDENCE" || fail "maximum-scene chunk count drifted"
grep -Fq '"max_chunk_bytes": 1048576' "$EVIDENCE" || fail "a replay chunk left the wire bound"
grep -Fq '"chunk_ceiling_bytes": 1048576' "$EVIDENCE" || fail "the chunk ceiling drifted"
grep -Fq '"frame_ceiling_bytes": 67108864' "$EVIDENCE" || fail "the IPC frame ceiling drifted"
grep -Fq '"oversized_records": 0' "$EVIDENCE" || fail "a replay record exceeded the chunk ceiling"
grep -Fq '"invalid_records": 0' "$EVIDENCE" || fail "a planned replay record failed validation"

# A late attach receives the whole scene before any delta, under one generation,
# with the commit last — the client can never observe a partial scene.
grep -Fq '"suppressed_live_deliveries": 0' "$EVIDENCE" \
    || fail "a sink with no scene received live deltas"
grep -Fq '"single_generation": true' "$EVIDENCE" || fail "the replay burst mixed generations"
grep -Fq '"commit_is_last": true' "$EVIDENCE" || fail "a record followed the publishing commit"
grep -Fq '"wire_order": "replay_begin,replay_definition,replay_chunk,replay_placement,replay_commit"' \
    "$EVIDENCE" || fail "the attaching viewer observed the wrong wire order"
grep -Fq '"replayed_definitions": 1' "$EVIDENCE" || fail "the late attach replayed no definition"
grep -Fq '"replayed_placements": 1' "$EVIDENCE" || fail "the late attach replayed no placement"

# A saturated viewer sheds this session's queued output, stops receiving deltas
# entirely, and one fresh combined replay clears the debt.
grep -Fq '"flooded_bytes": 8388608' "$EVIDENCE" || fail "the overflow flood changed size"
grep -Fq '"queue_shed_ceiling_bytes": 4194304' "$EVIDENCE" || fail "the shed ceiling drifted"
grep -Fq '"debt_after_overflow": 1' "$EVIDENCE" \
    || fail "shedding the backlog left the sink out of replay debt"
grep -Fq '"live_delivered_while_dirty": 0' "$EVIDENCE" \
    || fail "a replay-dirty sink kept receiving live deltas"
grep -Fq '"recovery_viewers": 1' "$EVIDENCE" || fail "the recovery burst missed its viewer"
grep -Fq '"debt_after_recovery": 0' "$EVIDENCE" || fail "the recovery burst left debt behind"
grep -Fq '"recovered_commit_seen": true' "$EVIDENCE" \
    || fail "the recovered viewer never read a replay commit"

# A viewerless session retains its scene and owes nobody anything; two viewers
# that join later are served by one plan whose cost does not depend on them.
grep -Fq '"viewerless_live_delivered": 0' "$EVIDENCE" \
    || fail "a viewerless session delivered records"
grep -Fq '"viewerless_debt": 0' "$EVIDENCE" || fail "a viewerless session accrued replay debt"
grep -Fq '"viewerless_definitions": 1' "$EVIDENCE" \
    || fail "a viewerless session dropped its canonical definitions"
grep -Fq '"viewerless_placements": 1' "$EVIDENCE" \
    || fail "a viewerless session dropped its canonical placements"
grep -Fq '"simultaneous_viewers": 2' "$EVIDENCE" || fail "one plan did not serve both viewers"
grep -Fq '"plans_built": 1' "$EVIDENCE" || fail "the server planned the scene more than once"
grep -Fq '"counters_independent_of_viewers": true' "$EVIDENCE" \
    || fail "the planned scene changed with the number of viewers"

# No image payload may reach the evidence.
if grep -qE '"(payload|bytes|data|rgba|pixels)": *\[' "$EVIDENCE"; then
    fail "evidence embedded image payload data"
fi

echo "PASS: combined terminal image replay at $EVIDENCE"
