#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Decode Spike Verification]]
set -euo pipefail

CONTRACT=/tests/fixtures/terminal-images/contract.json
OUTPUT_DIR=/output/terminal-images
EVIDENCE="$OUTPUT_DIR/decode-spike-evidence.json"
DECISION="$OUTPUT_DIR/decoder-decision.md"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

mkdir -p "$OUTPUT_DIR"
scribe-test decode-spike --contract "$CONTRACT" --evidence "$EVIDENCE"

[ -s "$EVIDENCE" ] || fail "decode spike did not write evidence"
grep -Fq '"all_passed": true' "$EVIDENCE" || fail "evidence does not pass"
grep -Fq '"decision": "conditional_go"' "$EVIDENCE" || fail "decision is missing"

for case_id in \
    dimensions_max \
    dimensions_max_plus_one \
    fallible_allocation \
    cooperative_cancellation \
    decode_deadline \
    work_max_plus_one \
    zlib_bomb \
    png_valid \
    png_bomb \
    png_max_plus_one \
    sixel_gradual_growth
do
    grep -Fq "\"id\": \"$case_id\"" "$EVIDENCE" \
        || fail "evidence omits $case_id"
done

temporary="$DECISION.tmp.$$"
cat >"$temporary" <<'DECISION'
# Bounded Terminal-Image Decoder Decision

Decision: **conditional go** for production decoder tasks.

- GO: wrap `flate2 1.1.9` low-level `Decompress` in a Scribe-owned
  4,096-work-unit loop. Charge consumed input and produced output, check
  cancellation/deadline, reject projected output, then grow fallibly.
- FORK REQUIRED: vendor only `icy_sixel 0.5.0` decoder code. Remove encoder
  and `quantette`; add caller-owned `DecodeLimits`, checked canvas growth,
  fallible allocation, cumulative work, cancellation, and deadline hooks.
- FORK REQUIRED: vendor only `png 0.18.1` decoder core. Its allocation limit
  is documented as best effort and it exposes no Scribe work/cancel callback.
  Add a step hook at compressed-input, inflated-output, row-unfilter, and
  pixel-conversion boundaries; reject APNG and ancillary text/profile data.
- NO-GO: generic `image` decoding, stock `icy_sixel`, stock `png` decoding,
  C decoders, and any decoder that can select non-PNG formats or indirect
  resources.

Evidence is in `decode-spike-evidence.json`. It records frozen contract limits,
typed outcomes, allocation peaks, work/deadline checks, max/max-plus-one
dimensions, zlib/PNG bombs, and gradual Sixel growth.
DECISION
mv "$temporary" "$DECISION"

grep -Fq 'Decision: **conditional go**' "$DECISION" || fail "decision artifact is invalid"
echo "PASS: bounded decoder evidence and decision written to $OUTPUT_DIR"
