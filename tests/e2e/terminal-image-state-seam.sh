#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Session State Seam#Docker Evidence Entry Point]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images/ipc.json
EVIDENCE=/output/terminal-images/state-seam.json
IPC_EVIDENCE=/output/terminal-images/state-seam-ipc.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-state-seam --evidence "$EVIDENCE"
scribe-test terminal-image-ipc --fixtures "$FIXTURES" --output "$IPC_EVIDENCE"

[ -s "$EVIDENCE" ] || fail "state seam probe did not write evidence"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "state seam evidence does not pass"
grep -Fq '"engine": "scribe-server process_pty_reader_ingress"' "$EVIDENCE" \
    || fail "probe did not exercise shared production reader ingress"
grep -Fq '"shared": true' "$EVIDENCE" \
    || fail "process policy is not shared"
grep -Fq '"payload_free": true' "$EVIDENCE" \
    || fail "pending metadata includes payload"
grep -Fq '"typed_rejection": "pass"' "$EVIDENCE" \
    || fail "sequence exhaustion was not typed"
grep -Fq '"state_unchanged": "pass"' "$EVIDENCE" \
    || fail "sequence rejection mutated terminal image state"
grep -Fq '"offset_unconsumed": "pass"' "$EVIDENCE" \
    || fail "sequence rejection consumed framed input"
grep -Fq '"large_transfer_bytes": 8388608' "$EVIDENCE" \
    || fail "large split-transfer evidence is missing"
grep -Fq '"split_read_bytes": 65536' "$EVIDENCE" \
    || fail "split-transfer reads did not use the production read size"
grep -Fq '"direct_reads": 130' "$EVIDENCE" \
    || fail "large transfer did not report exact direct framing work"
grep -Fq '"speculative_clone_reads": 0' "$EVIDENCE" \
    || fail "normal large reads deep-cloned speculative framing state"
grep -Fq '"client_delivery_calls": 2' "$EVIDENCE" \
    || fail "controlled client path was not invoked exactly once per read"
grep -Fq '"term_feed_calls": 2' "$EVIDENCE" \
    || fail "controlled Term path was not invoked exactly once per read"
grep -Fq '"matching_digest": true' "$EVIDENCE" \
    || fail "client and Term paths observed different effective bytes"
grep -Fq '"live_image_fanout": "disconnected"' "$EVIDENCE" \
    || fail "shared ingress performed live image fanout"
grep -Fq '"legacy_none_omitted_and_defaulted": true' "$IPC_EVIDENCE" \
    || fail "legacy MessagePack defaults drifted"

echo "PASS: production terminal-image state seam and legacy bytes"
