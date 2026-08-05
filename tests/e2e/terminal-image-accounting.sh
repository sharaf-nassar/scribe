#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Storage Accounting#Docker Evidence Entry Point]]
set -euo pipefail

EVIDENCE=/output/terminal-images/accounting.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

scribe-test terminal-image-accounting --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "accounting probe did not write evidence"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "accounting evidence does not pass"
grep -Fq '"metric": "requested live storage with allocator-observed retained capacity"' "$EVIDENCE" \
    || fail "evidence claims the wrong storage metric"
grep -Fq '"kitty_max_plus_one": "pass"' "$EVIDENCE" \
    || fail "Kitty max-plus-one did not reject"
grep -Fq '"sixel_max_plus_one": "pass"' "$EVIDENCE" \
    || fail "Sixel max-plus-one did not reject"
grep -Fq '"reservation_attempts": 14' "$EVIDENCE" \
    || fail "reservation attempts were not recorded"
grep -Fq '"allocator_attempts": 11' "$EVIDENCE" \
    || fail "rejected request allocated before reservation"
grep -Fq '"completed_requested": 8' "$EVIDENCE" \
    || fail "completed Kitty content was not retained"
grep -Fq '"required_peak": 1028' "$EVIDENCE" \
    || fail "replacement required peak drifted"
grep -Fq '"enforced_limit": 1027' "$EVIDENCE" \
    || fail "replacement failing limit drifted"
grep -Fq '"failed_growth_rollback": true' "$EVIDENCE" || fail "growth rollback failed"
grep -Fq '"failed_replacement_rollback": true' "$EVIDENCE" \
    || fail "replacement rollback failed"
grep -Fq '"typed_rejection": "process_limit"' "$EVIDENCE" \
    || fail "cross-session process pressure was not typed"
grep -Fq '"foreign_session_unchanged": true' "$EVIDENCE" \
    || fail "cross-session rejection mutated ownership"
grep -Fq '"paired_partial_charge_prevented": true' "$EVIDENCE" \
    || fail "paired ledger transaction leaked a partial charge"
grep -Fq '"mixed_rejections_unchanged": true' "$EVIDENCE" \
    || fail "mixed paired-ledger rejection changed state"
grep -Fq '"reserve_mixed_precedence": "internal_before_capacity_both_orderings"' "$EVIDENCE" \
    || fail "reserve paired-ledger precedence drifted"
grep -Fq '"reconcile_mixed_precedence": "internal_before_capacity_both_orderings"' "$EVIDENCE" \
    || fail "reconcile paired-ledger precedence drifted"
grep -Fq '"poisoned_ledger": "pass"' "$EVIDENCE" \
    || fail "poisoned ledger was not rejected without mutation"
grep -Fq '"observed": 7' "$EVIDENCE" \
    || fail "extra allocator-observed capacity was not charged"
grep -Fq '"framer_observed": 33' "$EVIDENCE" \
    || fail "framer capacity did not use the shared observer"
grep -Fq '"reconciliations": 12' "$EVIDENCE" \
    || fail "successful observed-capacity reconciliation was not recorded"
grep -Fq '"failed_reconcile_rollback": true' "$EVIDENCE" \
    || fail "Sixel observed-capacity rejection did not roll back"
grep -Fq '"storage_error": "session_limit"' "$EVIDENCE" \
    || fail "canonical replacement rejection did not use typed storage error"
grep -Fq '"event_release_exact": true' "$EVIDENCE" \
    || fail "rejected replacement event did not release exactly once"
grep -Fq '"candidate_error": "counter_overflow"' "$EVIDENCE" \
    || fail "candidate accounting error was hidden"
grep -Fq '"active_error": "allocation_failed"' "$EVIDENCE" \
    || fail "confirmed active accounting error was hidden"
grep -Fq '"canonical_error": "session_limit"' "$EVIDENCE" \
    || fail "canonical accounting error was hidden"
grep -Fq '"canonical_unchanged": true' "$EVIDENCE" \
    || fail "multi-event canonical state did not roll back"
grep -Fq '"state_unchanged": true' "$EVIDENCE" \
    || fail "multi-event definitions, placements, or counters changed"
grep -Fq '"ownership_unchanged": true' "$EVIDENCE" \
    || fail "multi-event retained ownership changed"
grep -Fq '"digests_unchanged": true' "$EVIDENCE" \
    || fail "multi-event retained buffers changed"
grep -Fq '"staged_release_exact": true' "$EVIDENCE" \
    || fail "multi-event staged leases did not release exactly"
grep -Fq '"allocation_class": "canonical_sixel"' "$EVIDENCE" \
    || fail "multi-event fault did not target canonical Sixel storage"
grep -Fq '"matching_allocation_attempts": 2' "$EVIDENCE" \
    || fail "multi-event fault did not reach second canonical allocation"
grep -Fq '"staged_before_failure": 1' "$EVIDENCE" \
    || fail "first canonical event was not staged before rejection"
grep -Fq '"targeted_rejection_fired": 1' "$EVIDENCE" \
    || fail "canonical allocation fault did not fire exactly once"
grep -Fq '"client_delivery_once": true' "$EVIDENCE" \
    || fail "storage-error bytes were not delivered once"
grep -Fq '"term_feed_once": true' "$EVIDENCE" \
    || fail "storage-error bytes were not fed once"
grep -Fq '"matching_digest": true' "$EVIDENCE" \
    || fail "storage-error ordinary bytes diverged"
grep -Fq '"rejection_callback_once": true' "$EVIDENCE" \
    || fail "typed storage rejection callback did not run exactly once"
grep -Fq '"rejection_payload_free": true' "$EVIDENCE" \
    || fail "storage rejection evidence was not payload-free"
grep -Fq '"final_process_current": 0' "$EVIDENCE" \
    || fail "process ownership did not return to zero"
grep -Fq '"external_release_applied": "pass"' "$EVIDENCE" \
    || fail "concurrent external release was rolled back"
grep -Fq '"failed_peak_unpublished": "pass"' "$EVIDENCE" \
    || fail "failed concurrent transaction published a peak"
grep -Fq '"invariant_error": "internal_invariant"' "$EVIDENCE" \
    || fail "concurrent invariant failure was remapped"
grep -Fq '"no_deadlock": "pass"' "$EVIDENCE" \
    || fail "concurrent release case deadlocked"
grep -Fq '"framing_event_metadata_peak": 480' "$EVIDENCE" \
    || fail "framing-event metadata peak drifted"
grep -Fq '"terminal_output_metadata_peak": 496' "$EVIDENCE" \
    || fail "terminal-output metadata peak drifted"
grep -Fq '"decoded_kitty_peak": 28' "$EVIDENCE" \
    || fail "decoded Kitty replacement peak drifted"
grep -Fq '"targeted_failure_class": "decoded_kitty"' "$EVIDENCE" \
    || fail "Kitty local rollback targeted the wrong class"
grep -Fq '"targeted_failure_occurrence": 2' "$EVIDENCE" \
    || fail "Kitty local rollback targeted the wrong occurrence"
grep -Fq '"global_max_minus_scope": "first_ingress_framing_peak"' "$EVIDENCE" \
    || fail "Kitty global max-minus scope drifted"
grep -Fq '"final_rollback_scope": "decoded_kitty_occurrence_2"' "$EVIDENCE" \
    || fail "Kitty final rollback scope drifted"
grep -Fq '"replacement_peak": 1272' "$EVIDENCE" \
    || fail "Sixel replacement peak drifted"
grep -Fq '"decoded_growth_overlap": 240' "$EVIDENCE" \
    || fail "Sixel geometric growth overlap drifted"
grep -Fq '"decoded_compaction_overlap": 288' "$EVIDENCE" \
    || fail "Sixel compaction overlap drifted"
grep -Fq '"body_digest": 2489256947087179384' "$EVIDENCE" \
    || fail "Sixel body digest drifted"
grep -Fq '"decoded_digest": 13492316921505547432' "$EVIDENCE" \
    || fail "Sixel decoded digest drifted"
grep -Fq '"exact_limit": 1272' "$EVIDENCE" \
    || fail "Sixel exact limit drifted"
grep -Fq '"global_max_minus_stage": "terminal_outputs_publication_reserve"' "$EVIDENCE" \
    || fail "Sixel max-minus stage drifted"
grep -Fq '"failed_reconcile_target_class": "terminal_outputs"' "$EVIDENCE" \
    || fail "observed-capacity rejection targeted wrong class"
grep -Fq '"failed_reconcile_target_occurrence": 1' "$EVIDENCE" \
    || fail "observed-capacity rejection targeted wrong occurrence"
grep -Fq '"failed_reconcile_reservations": 15' "$EVIDENCE" \
    || fail "observed-capacity reservation telemetry drifted"
grep -Fq '"failed_reconcile_reconciliations": 11' "$EVIDENCE" \
    || fail "observed-capacity reconcile telemetry drifted"
grep -Fq '"process_current_at_limit": 29' "$EVIDENCE" \
    || fail "cross-session retained current drifted"
grep -Fq '"required_peak": 1032' "$EVIDENCE" \
    || fail "cross-session required peak drifted"
grep -Fq '"enforced_limit": 1031' "$EVIDENCE" \
    || fail "cross-session enforced limit drifted"
grep -Fq '"rejection_reservation_delta": 10' "$EVIDENCE" \
    || fail "cross-session rejection reservations drifted"
grep -Fq '"rejection_allocator_delta": 7' "$EVIDENCE" \
    || fail "cross-session rejection allocations drifted"
grep -Fq '"detached_requested": 32' "$EVIDENCE" \
    || fail "detached command body size drifted"
grep -Fq '"detached_outputs_requested": 496' "$EVIDENCE" \
    || fail "detached output metadata size drifted"
grep -Fq '"detached_total_requested": 528' "$EVIDENCE" \
    || fail "detached package size drifted"
grep -Fq '"in_flight_process_peak": 1124' "$EVIDENCE" \
    || fail "concurrent in-flight peak no longer exceeds the committed peak"
grep -Fq '"process_current_before": 532' "$EVIDENCE" \
    || fail "concurrent initial current drifted"
grep -Fq '"process_current_after_external_release": 596' "$EVIDENCE" \
    || fail "concurrent provisional current drifted"
grep -Fq '"process_current_after_failure": 4' "$EVIDENCE" \
    || fail "concurrent rollback current drifted"
grep -Fq '"process_peak_after_failure": 1016' "$EVIDENCE" \
    || fail "concurrent committed peak drifted"
grep -Fq '"class_states_exact": "pass"' "$EVIDENCE" \
    || fail "concurrent class snapshots drifted"
grep -Fq '"aggregate_encoded_bytes": 5464' "$EVIDENCE" \
    || fail "multi-chunk aggregate size drifted"
grep -Fq '"aggregate_split_success": true' "$EVIDENCE" \
    || fail "multi-chunk aggregate decode failed"
grep -Fq '"individual_chunk_rejection": "kitty_chunk_payload_bytes"' "$EVIDENCE" \
    || fail "oversized Kitty chunk was not rejected"
grep -Fq '"chunk_count_rejection": "chunks_per_transfer"' "$EVIDENCE" \
    || fail "Kitty chunk-count boundary drifted"
grep -Fq '"first_action_preserved": true' "$EVIDENCE" \
    || fail "first Kitty action was not preserved"
grep -Fq '"first_ids_preserved": true' "$EVIDENCE" \
    || fail "first Kitty ids were not preserved"
grep -Fq '"first_quiet_preserved": true' "$EVIDENCE" \
    || fail "first Kitty quiet control was not preserved"
grep -Fq '"query_canonical_retained": 0' "$EVIDENCE" \
    || fail "Kitty query retained canonical storage"
grep -Fq '"pending_after_final": 0' "$EVIDENCE" \
    || fail "Kitty transfer remained pending after final chunk"
grep -Fq '"equal_repeats_accepted": true' "$EVIDENCE" \
    || fail "equal Kitty continuation controls were rejected"
grep -Fq '"conflicting_controls_rejected": true' "$EVIDENCE" \
    || fail "conflicting Kitty continuation controls were merged"
grep -Fq '"query_boundary_ordered": true' "$EVIDENCE" \
    || fail "Kitty query boundaries were reordered"
grep -Fq '"query_publication_count": 0' "$EVIDENCE" \
    || fail "Kitty query published image state"
grep -Fq '"candidate_exact_rollback": true' "$EVIDENCE" \
    || fail "candidate framer state did not roll back exactly"
grep -Fq '"candidate_retry_events": 1' "$EVIDENCE" \
    || fail "candidate retry duplicated publication"
grep -Fq '"active_exact_rollback": true' "$EVIDENCE" \
    || fail "active framer state did not roll back exactly"
grep -Fq '"active_retry_events": 1' "$EVIDENCE" \
    || fail "active retry duplicated publication"
grep -Fq '"eof_exact_rollback": true' "$EVIDENCE" \
    || fail "EOF framer state did not roll back exactly"
grep -Fq '"eof_retry_events": 1' "$EVIDENCE" \
    || fail "EOF retry duplicated publication"
grep -Fq '"no_duplicate_publication": true' "$EVIDENCE" \
    || fail "framer retry published duplicate events"
for format in raw_rgba zlib_rgba png sixel; do
    grep -Fq "\"id\": \"$format\"" "$EVIDENCE" \
        || fail "missing $format production format evidence"
done
grep -Fq '"measured_peak": 1178' "$EVIDENCE" \
    || fail "PNG production peak drifted"
grep -Fq '"decoded_digest": 11588189572237274325' "$EVIDENCE" \
    || fail "raw/zlib RGBA digest drifted"
grep -Fq '"decoded_digest": 11588189572237274452' "$EVIDENCE" \
    || fail "PNG RGBA digest drifted"
grep -Fq '"decoded_digest": 9107566641596638602' "$EVIDENCE" \
    || fail "Sixel format digest drifted"
grep -Fq '"input_bytes": 128' "$EVIDENCE" \
    || fail "metadata hostile-input size drifted"
grep -Fq '"event_requested_peak": 46080' "$EVIDENCE" \
    || fail "metadata event peak drifted"
grep -Fq '"output_requested_peak": 47616' "$EVIDENCE" \
    || fail "metadata output peak drifted"
grep -Fq '"measured_total_peak": 78400' "$EVIDENCE" \
    || fail "metadata total peak drifted"
grep -Fq '"max_minus_one_rejection": "session_limit"' "$EVIDENCE" \
    || fail "format or metadata max-minus rejection drifted"
grep -Fq '"rollback_unchanged": true' "$EVIDENCE" \
    || fail "format or metadata rollback drifted"

grep -Fq '"charged_while_iterating": true' "$EVIDENCE" \
    || fail "consumed event vector released ownership before its allocation"
grep -Fq '"charged_after_partial_drain": true' "$EVIDENCE" \
    || fail "partially drained event vector released ownership early"
grep -Fq '"released_after_iterator_drop": true' "$EVIDENCE" \
    || fail "consumed event vector did not release ownership exactly once"
grep -Fq '"final_controls_preserved": true' "$EVIDENCE" \
    || fail "split Kitty final boundary lost its first-command controls"
grep -Fq '"final_presence_preserved": true' "$EVIDENCE" \
    || fail "split Kitty final boundary lost its first-command control presence"
grep -Fq '"accounted_before_allocation": true' "$EVIDENCE" \
    || fail "grid observations allocated outside the paired ledger"
grep -Fq '"released_after_commit_drop": true' "$EVIDENCE" \
    || fail "grid observation storage did not release with its commit"
grep -Fq '"rejected_ledger_zero": true' "$EVIDENCE" \
    || fail "refused grid observation storage left the ledger charged"
grep -Fq '"sixel_decoded_peak": 0' "$EVIDENCE" \
    || fail "work-refused Sixel decode reserved storage before admission"
grep -Fq '"no_storage_before_admission": true' "$EVIDENCE" \
    || fail "decoder initialization ran before work-budget admission"

echo "PASS: exact Kitty and Sixel requested-storage accounting"
