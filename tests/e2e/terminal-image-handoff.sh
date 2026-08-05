#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Handoff#Docker Evidence Entry Point]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images
EVIDENCE=/output/terminal-images/handoff.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-handoff --fixtures "$FIXTURES" --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "handoff probe did not write evidence"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "handoff evidence does not pass"
grep -Fq '"engine": "scribe-server terminal image handoff"' "$EVIDENCE" \
    || fail "probe did not exercise the production handoff seam"
grep -Fq '"payload_free": true' "$EVIDENCE" \
    || fail "handoff evidence is not payload-free"

# Partial APC and DCS: reads paused inside a control string and the successor
# consumed its remainder as the same image command.
grep -Fq '"partial_protocol": "kitty"' "$EVIDENCE" \
    || fail "no partial APC string crossed the handoff"
grep -Fq '"partial_protocol": "sixel"' "$EVIDENCE" \
    || fail "no partial DCS string crossed the handoff"
grep -Fq '"partial_apc_resumes": "pass"' "$EVIDENCE" \
    || fail "the partial APC session did not resume"
grep -Fq '"partial_dcs_resumes": "pass"' "$EVIDENCE" \
    || fail "the partial DCS session did not resume"

# Kitty chunk accumulation: an in-flight transfer survived with its normalized
# bytes and finished on the successor.
grep -Fq '"kitty_chunk_accumulation_resumes": "pass"' "$EVIDENCE" \
    || fail "an in-flight chunked transfer did not resume"
grep -Fq '"pending_transfer": true' "$EVIDENCE" \
    || fail "the chunked pause carried no in-flight transfer"

# Ordered resume with no loss: every resumed session matched its no-handoff
# control on cursor, scene, and published records.
grep -Fq '"ordered_resume_without_loss": "pass"' "$EVIDENCE" \
    || fail "a resumed session diverged from its control"
grep -Fq '"scene_matches_control": true' "$EVIDENCE" \
    || fail "a restored canonical scene differs from its control"
grep -Fq '"published_matches_control": true' "$EVIDENCE" \
    || fail "a resumed read published different records than its control"
! grep -Fq '"scene_matches_control": false' "$EVIDENCE" \
    || fail "a restored canonical scene differs from its control"

# Maximum scene: bounded chunks, fits the payload ceiling, and an oversized
# scene is dropped whole rather than truncated.
grep -Fq '"max_scene_stays_bounded": "pass"' "$EVIDENCE" \
    || fail "the maximum scene exceeded its handoff bounds"
grep -Fq '"handoff_image_ceiling_bytes": 134217728' "$EVIDENCE" \
    || fail "the handoff image ceiling changed without evidence"
grep -Fq '"max_chunk_bytes": 1048576' "$EVIDENCE" \
    || fail "the maximum scene did not chunk at the wire ceiling"
grep -Fq '"oversized_records": 0' "$EVIDENCE" \
    || fail "the maximum scene produced an oversized record"
grep -Fq '"invalid_records": 0' "$EVIDENCE" \
    || fail "the maximum scene produced an invalid record"
grep -Fq '"dropped_scenes": 1' "$EVIDENCE" \
    || fail "an oversized scene was not dropped"
grep -Fq '"dropped_session_records": 2' "$EVIDENCE" \
    || fail "a dropped scene emitted a partial burst"
grep -Fq '"truncated_payload_refused": true' "$EVIDENCE" \
    || fail "a truncated handoff burst was accepted"
grep -Fq '"unbacked_placement_refused": true' "$EVIDENCE" \
    || fail "a placement without its definition was accepted"

# Old-to-new restore, new-to-old rollback refusal, and downgrade config.
grep -Fq '"image_payload_version": 7' "$EVIDENCE" \
    || fail "an image-carrying payload did not declare the image version"
grep -Fq '"image_free_payload_version": 6' "$EVIDENCE" \
    || fail "an image-free payload did not declare the pre-image version"
grep -Fq '"image_free_payload_omits_key": true' "$EVIDENCE" \
    || fail "an image-free payload still carried the image key"
grep -Fq '"old_to_new_restores_empty": true' "$EVIDENCE" \
    || fail "a pre-image payload did not restore as an empty scene"
grep -Fq '"new_to_old_refused": true' "$EVIDENCE" \
    || fail "a pre-image receiver accepted an image payload"
grep -Fq '"downgraded_payload_accepted": true' "$EVIDENCE" \
    || fail "the downgraded payload was refused"
grep -Fq '"downgrade_exports_nothing": true' "$EVIDENCE" \
    || fail "disabling images still exported image state"

echo "PASS: terminal image state persists through server handoff"
