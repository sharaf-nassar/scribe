#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Image Replies and Viewer Sharing#Docker Evidence Entry Point]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images
EVIDENCE=/output/terminal-images/replies-sharing.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-replies-sharing --fixtures "$FIXTURES" --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "sharing probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "sharing evidence does not pass"
grep -Fq '"engine": "scribe-server image replies and capable-sink fanout"' "$EVIDENCE" \
    || fail "the probe did not run through the production sharing seam"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence claims retained payloads"

# Every acceptance behavior maps to a named case.
for case in kitty_reply_before_da reply_exactly_once failure_quiet_suppression \
    da4_enablement kill_switch_transitions incapable_attach_refusal \
    latch_survives_detach zero_one_multiple_viewers controller_change detach; do
    grep -Fq "\"$case\": \"pass\"" "$EVIDENCE" || fail "case $case did not pass"
done

# A Kitty result precedes the DA1 reply in the same read, and each is written
# exactly once. Replanning the same commit — what a reattach or replay would
# do — must not add a second PTY reply, and a disabled session owes none.
grep -Fq '"ordered_pty_writes": "kitty_ok,device_attributes"' "$EVIDENCE" \
    || fail "the Kitty result did not precede the DA1 reply"
grep -Fq '"kitty_replies": 1' "$EVIDENCE" || fail "the Kitty reply was not written once"
grep -Fq '"device_attributes_replies": 1' "$EVIDENCE" \
    || fail "the DA1 reply was not written once"
grep -Fq '"replayed_kitty_replies": 1' "$EVIDENCE" \
    || fail "replanning a committed read changed its reply count"
grep -Fq '"disabled_kitty_replies": 0' "$EVIDENCE" \
    || fail "a disabled session still answered a Kitty probe"

# The contract's quiet ladder applies to errors too: q=2 suppresses a failure
# reply, q=1 suppresses success only and must keep answering errors, and a q=
# operand that is not a defined level cannot silence anything.
grep -Fq '"q0_failure_replies": 1' "$EVIDENCE" || fail "q=0 swallowed a failure reply"
grep -Fq '"q1_failure_replies": 1' "$EVIDENCE" \
    || fail "q=1 suppressed a failure reply; it suppresses success only"
grep -Fq '"q2_failure_replies": 0' "$EVIDENCE" || fail "q=2 did not suppress the failure reply"
grep -Fq '"failure_code": "ENOSYS,ENOSYS"' "$EVIDENCE" \
    || fail "an unsupported transport did not answer ENOSYS at both loud levels"
grep -Fq '"unreadable_quiet_failure_replies": 1' "$EVIDENCE" \
    || fail "a failure whose quiet level was unreadable was silently swallowed"

# DA1 gains attribute 4 only while the capability is live, and every kill-switch
# transition is reported exactly once.
grep -Fq '\\u{1b}[?6;4c' "$EVIDENCE" || fail "enabled DA1 lacks Sixel attribute 4"
grep -Fq '"disabled_device_attributes": "\\u{1b}[?6c"' "$EVIDENCE" \
    || fail "disabled DA1 still advertises Sixel"
grep -Fq '"transitions": "disabled_cleared_latch,unchanged,enabled"' "$EVIDENCE" \
    || fail "kill-switch transitions are wrong or repeated"
grep -Fq '"cleared_latch_once": true' "$EVIDENCE" \
    || fail "disabling did not return the session to text-only-unlatched"
grep -Fq '"relatched_after_reenable": true' "$EVIDENCE" \
    || fail "a capable viewer could not re-latch after re-enable"

# Zero, one, and multiple viewers; a controller change; a detach.
grep -Fq '"zero_viewers_delivered": 0' "$EVIDENCE" \
    || fail "a viewerless session delivered records"
grep -Fq '"one_viewer_delivered": 1' "$EVIDENCE" || fail "one capable viewer was skipped"
grep -Fq '"multiple_viewers_delivered": 2' "$EVIDENCE" \
    || fail "multiple capable viewers were not all served"
grep -Fq '"multiple_viewers_received": "5,5"' "$EVIDENCE" \
    || fail "capable viewers did not each receive the burst exactly once"
grep -Fq '"incapable_viewer_received": 0' "$EVIDENCE" \
    || fail "an incapable sink received image records"
grep -Fq '"controller_change_delivered": 1' "$EVIDENCE" \
    || fail "a controller change did not re-point the sink set"
grep -Fq '"controller_change_displaced_received": 0' "$EVIDENCE" \
    || fail "displaced viewers kept receiving records"
grep -Fq '"detached_delivered": 0' "$EVIDENCE" || fail "a detached session delivered records"
grep -Fq '"detached_viewer_received": 0' "$EVIDENCE" \
    || fail "a detached viewer still received records"

# An incapable viewer is refused with a typed mismatch, and the latch is
# session state: viewer count and detach never change it.
grep -Fq '"incapable_refusals": 2' "$EVIDENCE" \
    || fail "an incapable viewer was not refused before and after detach"
grep -Fq '"capable_admissions": 4' "$EVIDENCE" || fail "a capable viewer was refused"
grep -Fq '"latch_is_idempotent": true' "$EVIDENCE" \
    || fail "a second viewer changed the latched subset"
grep -Fq '"survives_detach": true' "$EVIDENCE" || fail "the latch did not survive detach"
grep -Fq '"unlatched_admission": "admits_any_viewer"' "$EVIDENCE" \
    || fail "an ordinary text session refused a viewer"

# No image payload may reach the evidence.
if grep -qE '"(payload|bytes|data|rgba|pixels)": *\[' "$EVIDENCE"; then
    fail "evidence embedded image payload data"
fi

echo "PASS: terminal image replies and viewer sharing at $EVIDENCE"
