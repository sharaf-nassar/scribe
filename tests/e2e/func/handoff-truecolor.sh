#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Handoff of a truecolor-dense screen (audit finding #4, US2-2).
#
# Every painted cell carries its own 24-bit fg and bg, so the replay encoder
# emits a fresh SGR run per cell and the ANSI stream inflates far past the
# eight-bytes-per-declared-cell bound the decoder used to allocate. That bound
# failed the decode, restore_from_handoff fell back to a v5 sender's absent
# legacy snapshot, and the session came back blank. The decode is streamed under
# an absolute 64 MiB ceiling now, so the colors must survive the upgrade.

# --- Phase 1: Paint a truecolor-dense screen ---
{
    for ((row = 0; row < 60; row++)); do
        for ((col = 0; col < 100; col++)); do
            printf '\033[38;2;%d;%d;%dm\033[48;2;%d;%d;%dmX' \
                $(((col * 7) % 256)) $(((row * 13) % 256)) $(((col + row) % 256)) \
                $(((row * 3) % 256)) $(((col * 5) % 256)) $(((col * row) % 256))
        done
        printf '\033[0m\r\n'
    done
} >/output/truecolor.ans

scribe-test send "$SESSION" 'cat /output/truecolor.ans\n'
scribe-test wait-idle "$SESSION" --ms 500
scribe-test send "$SESSION" 'echo truecolor-marker\n'
scribe-test wait-output "$SESSION" "truecolor-marker"
echo "PHASE 1 PASS: truecolor screen painted"

# --- Phase 2: Confirm the pre-upgrade screen really is truecolor-dense ---
scribe-test snapshot "$SESSION" /output/pre-truecolor.json
PRE_RGB=$(grep -c '"Rgb"' /output/pre-truecolor.json || true)
if [ "$PRE_RGB" -lt 500 ]; then
    echo "PHASE 2 FAIL: fixture is not dense enough ($PRE_RGB truecolor cells)"
    exit 1
fi
echo "PHASE 2 PASS: $PRE_RGB truecolor cells on screen before the upgrade"

SAVED_SESSION="$SESSION"

# --- Phase 3: Hot-reload the server (fd handoff carries the replay) ---
scribe-test daemon stop
scribe-test server upgrade
scribe-test daemon start
scribe-test session attach "$SAVED_SESSION"
echo "PHASE 3 PASS: reattached to $SAVED_SESSION after hot-reload"

# --- Phase 4: The restored screen must still be the painted one ---
scribe-test snapshot "$SAVED_SESSION" /output/post-truecolor.json
POST_RGB=$(grep -c '"Rgb"' /output/post-truecolor.json || true)
if [ "$POST_RGB" -lt $((PRE_RGB / 2)) ]; then
    echo "PHASE 4 FAIL: truecolor content lost across handoff ($PRE_RGB -> $POST_RGB)"
    exit 1
fi
echo "PHASE 4 PASS: $POST_RGB truecolor cells survived (was $PRE_RGB)"

# --- Phase 5: The session is content, not a blank grid ---
CELLS=$(grep -oP '"c": "."' /output/post-truecolor.json | cut -d'"' -f4 | tr -d '\n')
if ! echo "$CELLS" | grep -qF "truecolor-marker"; then
    echo "PHASE 5 FAIL: screen content lost after hot-reload"
    echo "  (first 200 chars of cell content: ${CELLS:0:200})"
    exit 1
fi
echo "PHASE 5 PASS: screen content preserved after hot-reload"

# --- Phase 6: The session is still live ---
scribe-test send "$SAVED_SESSION" 'echo after-truecolor-handoff\n'
scribe-test wait-output "$SAVED_SESSION" "after-truecolor-handoff"
echo "PHASE 6 PASS: session still usable after hot-reload"

echo "PASS: truecolor handoff test completed"
