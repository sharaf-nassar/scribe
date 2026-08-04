#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Observer Parity#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/observer-parity.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-observer-parity --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "observer parity probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "observer evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "observer evidence does not pass"
grep -Fq '"engine": "scribe-server real Term observer"' "$EVIDENCE" \
    || fail "probe did not use production observer"
grep -Fq '"alacritty_terminal": "0.26.0-rc1"' "$EVIDENCE" \
    || fail "probe did not pin the production Alacritty version"
grep -Fq '"one_processor": true' "$EVIDENCE" || fail "probe used a second parser"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence retained payload data"
grep -Fq '"wrap_pending_and_image_move": "pass"' "$EVIDENCE" || fail "wrap case missing"
grep -Fq '"save_restore_per_grid": "pass"' "$EVIDENCE" || fail "save/restore case missing"
grep -Fq '"margins_and_scroll": "pass"' "$EVIDENCE" || fail "scroll case missing"
grep -Fq '"ed2_half_open_scope": "pass"' "$EVIDENCE" || fail "ED2 case missing"
grep -Fq '"ed1_pinned_semantics": "pass"' "$EVIDENCE" || fail "ED1 case missing"
grep -Fq '"alternate_1049": "pass"' "$EVIDENCE" || fail "1049 case missing"
grep -Fq '"split_reads": "pass"' "$EVIDENCE" || fail "split-read case missing"
grep -Fq '"same_read_chronology": "pass"' "$EVIDENCE" || fail "chronology case missing"
grep -Fq '"same_span_live_wrap_mode": "pass"' "$EVIDENCE" || fail "live-mode case missing"
grep -Fq '"deccolm_same_span": "pass"' "$EVIDENCE" || fail "DECCOLM case missing"
grep -Fq '"input_width_scroll_paths": "pass"' "$EVIDENCE" \
    || fail "input-width case missing"
grep -Fq '"ordered_boundary_cuts": "pass"' "$EVIDENCE" \
    || fail "ordered-boundary case missing"
grep -Fq '"image_error_observed_once": "pass"' "$EVIDENCE" \
    || fail "image rejection case missing"
grep -Fq '"synchronized_update_timeout": "pass"' "$EVIDENCE" \
    || fail "sync-timeout case missing"
grep -Fq '"resize_active_and_inactive": "pass"' "$EVIDENCE" \
    || fail "both-grid resize case missing"

echo "PASS: production Alacritty image observer parity"
