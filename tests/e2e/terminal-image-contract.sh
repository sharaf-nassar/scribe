#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Contract Verification]]
set -euo pipefail

CONTRACT=/tests/fixtures/terminal-images/contract.json
FIXTURES=/tests/fixtures/terminal-images/fixtures.tsv
ROOT=/tests
OUTPUT=/output/terminal-images/contract.json

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ -f "$CONTRACT" ] || fail "missing machine-readable contract"
[ -f "$FIXTURES" ] || fail "missing owned fixture manifest"

while IFS= read -r expected; do
    [ -z "$expected" ] && continue
    grep -Fqx "$expected" "$CONTRACT" \
        || fail "contract value drifted: $expected"
done <<'VALUES'
  "contract_version": "terminal-images-v1",
    "max_control_string_bytes": 16777216,
    "max_kitty_chunk_payload_bytes": 4096,
    "max_chunks_per_transfer": 32768,
    "max_accumulated_encoded_bytes": 89478488,
    "max_base64_decoded_bytes": 67108864,
    "max_inflated_bytes": 67108864,
    "max_width_pixels": 4096,
    "max_height_pixels": 4096,
    "max_pixels": 16777216,
    "max_canonical_rgba_bytes": 67108864,
    "max_images_per_session": 128,
    "max_placements_per_session": 1024,
    "max_session_retained_cpu_bytes": 134217728,
    "max_view_projected_gpu_bytes": 268435456,
    "max_process_retained_bytes": 536870912,
    "max_concurrent_decodes": 2,
    "max_decode_queue_depth": 8,
    "max_decode_queue_bytes": 134217728,
    "max_work_units_per_command": 134217728,
    "max_queue_wait_ms": 1000,
    "max_decode_ms": 2000,
    "max_replay_chunk_bytes": 1048576,
    "deadline_check_interval_work_units": 4096
VALUES

for needle in \
    '"formats": [24, 32, 100]' \
    '"transports": ["direct"]' \
    '"placement": ["classic", "unicode_placeholder"]' \
    '"discovery": "da1_attribute_4_when_runtime_enabled"' \
    '"80_reset": "cursor_anchor_scroll_and_advance"' \
    '"80_set": "page_origin_crop_cursor_unchanged"' \
    '"capability_mismatch"' \
    '"decode_deadline_exceeded"' \
    '"version": "v26.5.6"' \
    '"version": "1.18.2"' \
    '"version": "6.0.3"'
do
    grep -Fq "$needle" "$CONTRACT" || fail "missing frozen contract item: $needle"
done

fixture_count=0
while IFS=$'\t' read -r id path expect extra; do
    [ -n "$id" ] || fail "empty fixture id"
    [ -z "${extra:-}" ] || fail "unexpected fixture manifest column for $id"
    case "$path" in
        fixtures/terminal-images/*.hex) ;;
        *) fail "fixture escapes owned directory: $path" ;;
    esac

    file="$ROOT/$path"
    [ -f "$file" ] || fail "missing fixture $path"
    hex=$(tr -d '\n' <"$file")
    [ -n "$hex" ] || fail "empty fixture $path"
    printf '%s' "$hex" | grep -Eq '^[0-9a-f]+$' \
        || fail "fixture is not lowercase ASCII hex: $path"
    [ $(( ${#hex} % 2 )) -eq 0 ] || fail "fixture has half-byte hex: $path"

    grep -Fq "\"id\": \"$id\"" "$CONTRACT" \
        || fail "contract omits fixture id $id"
    grep -Fq "\"path\": \"$path\"" "$CONTRACT" \
        || fail "contract omits fixture path $path"
    grep -Fq "\"expect\": \"$expect\"" "$CONTRACT" \
        || fail "contract omits fixture expectation $expect"
    fixture_count=$((fixture_count + 1))
done <"$FIXTURES"

[ "$fixture_count" -eq 10 ] || fail "expected 10 owned fixtures, found $fixture_count"
duplicates=$(cut -f1 "$FIXTURES" | sort | uniq -d)
[ -z "$duplicates" ] || fail "duplicate fixture ids: $duplicates"

mkdir -p "$(dirname "$OUTPUT")"
temporary="$OUTPUT.tmp.$$"
cp "$CONTRACT" "$temporary"
mv "$temporary" "$OUTPUT"

cmp -s "$CONTRACT" "$OUTPUT" || fail "evidence copy differs from contract"
echo "PASS: terminal image v1 contract, limits, applications, and fixtures are frozen"
