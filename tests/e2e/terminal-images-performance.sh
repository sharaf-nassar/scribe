#!/bin/bash
# e2e-timeout: 900
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Image Performance and Resource Review#Server Measurement Pass]]
set -euo pipefail

# The server-side half of the clarification 7C measurement pass.
#
# Clarification 7C forbids inventing a numeric performance threshold, so this
# script gates on nothing it measures. It records text throughput, input
# latency, server CPU, decode latency, and retained bytes for the same session
# with the image path off and then on, and leaves the material-regression call
# to the human review in specs/020-terminal-images/performance-review.md.
#
# What it does still assert is the frozen security boundary: the hard numeric
# ImageLimits ceilings are not performance goals and must reject over-limit
# input even after the measurement workload has run.

OUT=/output/terminal-images
EVIDENCE="$OUT/performance.json"
SERVER_LOG="$OUT/performance-server.log"
WORK=/tmp/terminal-images-performance
CONFIG="$HOME/.config/scribe/config.toml"
mkdir -p "$OUT" "$WORK"
: >"$SERVER_LOG"

# The fixed workload: sixteen 256x256 direct RGB transfers, each large enough
# to be chunked at the frozen 4096-byte ceiling and to charge 256 KiB of
# canonical RGBA, and all sixteen together far inside the 128 MiB session cap
# so what is measured is decode and retention rather than quota rejection.
IMAGE_PX=256
IMAGE_COUNT=16

fail() {
    echo "FAIL: $*" >&2
    tail -40 "$SERVER_LOG" >&2 2>/dev/null || true
    exit 1
}

# ---------------------------------------------------------------------------
# Server log readers, shared with the safety corpus: the tracing writer
# colorizes even into a file, so every assertion reads de-colorized text.
# ---------------------------------------------------------------------------
plain_log() { tail -n "+${1:-1}" "$SERVER_LOG" | sed 's/\x1b\[[0-9;]*m//g'; }

log_field_max() {
    local field="$1" value
    value=$(plain_log "${2:-1}" | sed -n "s/.* $field=\([0-9][0-9]*\).*/\1/p" | sort -n | tail -1)
    printf '%s' "${value:-0}"
}

log_lines() { wc -l <"$SERVER_LOG" | tr -d ' '; }

wait_field_at_least() {
    local field="$1" want="$2" from="$3" deadline=$((SECONDS + ${4:-30}))
    until [ "$(log_field_max "$field" "$from")" -ge "$want" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.05
    done
}

wait_log() {
    local pattern="$1" from="$2" deadline=$((SECONDS + ${3:-20}))
    until plain_log "$from" | grep -qF "$pattern"; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

# ---------------------------------------------------------------------------
# Process meters. No procps in this image, so both readers walk /proc directly.
# ---------------------------------------------------------------------------
server_pid() {
    local dir pid
    for dir in /proc/[0-9]*; do
        pid=${dir#/proc/}
        [ "$(cat "$dir/comm" 2>/dev/null || true)" = scribe-server ] || continue
        printf '%s' "$pid"
        return 0
    done
    return 1
}

# utime + stime in clock ticks. `comm` is parenthesised and holds no space, so
# positional fields 14 and 15 are the two counters even under naive splitting.
cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }

status_kb() { sed -n "s/^$2:[[:space:]]*\([0-9][0-9]*\) kB/\1/p" "/proc/$1/status"; }

now_ms() { echo $(($(date +%s%N) / 1000000)); }

TICKS_PER_SEC=$(getconf CLK_TCK)

# ---------------------------------------------------------------------------
# Session drivers, shared with the safety corpus. Graphics replies are written
# back to the PTY as input, so every command discards the input line first, and
# every marker is assembled in the shell so the tty's echo of the typed keys is
# not mistaken for the command's output.
# ---------------------------------------------------------------------------
send_line() { scribe-test send "$SESSION" "\x15$1\n"; }

assert_session_alive() {
    send_line "echo ALIV''E-$1"
    scribe-test wait-output "$SESSION" "ALIVE-$1" --timeout 5000 ||
        fail "the session did not run a command after $1"
}

# ---------------------------------------------------------------------------
# Measurements. Every measurement publishes through a global or a file rather
# than a command substitution, so a workload that never completes fails the
# whole script instead of dying inside a subshell the caller cannot see.
# Nothing measured here is compared against a threshold.
# ---------------------------------------------------------------------------

# Wall time for the session to write TEXT_BYTES of ordinary text through the
# PTY reader, the terminal, and out to the harness sink.
TEXT_MS=0
measure_text_ms() {
    local tag="$1" start
    start=$(now_ms)
    send_line "cat $WORK/text.dat; echo TEXTDON''E-$tag"
    scribe-test wait-output "$SESSION" "TEXTDONE-$tag" --timeout 120000 ||
        fail "the text throughput burst never completed ($tag)"
    TEXT_MS=$(($(now_ms) - start))
}

# Twenty keystroke-to-echo round trips, left sorted in a file so the caller can
# read min, median, and max off the same list. Each round trip includes the
# harness CLI's own process spawn, which is a constant across both phases.
measure_input_ms() {
    local tag="$1" i start
    : >"$WORK/input-$tag.ms"
    for i in $(seq 1 20); do
        start=$(now_ms)
        send_line "echo PON''G-$tag-$i"
        scribe-test wait-output "$SESSION" "PONG-$tag-$i" --timeout 10000 ||
            fail "input round trip $i never echoed ($tag)"
        echo $(($(now_ms) - start)) >>"$WORK/input-$tag.ms"
    done
    sort -n -o "$WORK/input-$tag.sorted" "$WORK/input-$tag.ms"
}

nth() { sed -n "$2p" "$WORK/input-$1.sorted"; }

# ---------------------------------------------------------------------------
# Payloads.
# ---------------------------------------------------------------------------
# Ordinary text, no escape byte anywhere in it, so the throughput burst never
# enters the graphics framer's command path even when images are enabled.
head -c 1572864 /dev/urandom | base64 -w 76 >"$WORK/text.dat"
TEXT_BYTES=$(wc -c <"$WORK/text.dat")

# One chunked direct RGB transmit per image id. Non-final chunks are 4096
# base64 bytes — the frozen ceiling, and divisible by four as the framing rule
# requires — so the workload also exercises chunk accumulation while it is
# being timed.
head -c $((IMAGE_PX * IMAGE_PX * 3)) /dev/urandom | base64 -w 0 >"$WORK/payload.b64"
mapfile -t CHUNKS < <(fold -w 4096 "$WORK/payload.b64")

kitty_image() {
    local id="$1" out="$2" last=$((${#CHUNKS[@]} - 1)) index
    : >"$out"
    for index in "${!CHUNKS[@]}"; do
        if [ "$index" -eq 0 ]; then
            printf '\x1b_Ga=T,f=24,s=%s,v=%s,c=8,r=4,i=%s,q=2,m=1;%s\x1b\\' \
                "$IMAGE_PX" "$IMAGE_PX" "$id" "${CHUNKS[$index]}" >>"$out"
        elif [ "$index" -eq "$last" ]; then
            printf '\x1b_Gm=0;%s\x1b\\' "${CHUNKS[$index]}" >>"$out"
        else
            printf '\x1b_Gm=1;%s\x1b\\' "${CHUNKS[$index]}" >>"$out"
        fi
    done
}

kitty_image 1 "$WORK/image-1.bin"
: >"$WORK/batch.bin"
for id in $(seq 2 "$IMAGE_COUNT"); do
    kitty_image "$id" "$WORK/image-$id.bin"
    cat "$WORK/image-$id.bin" >>"$WORK/batch.bin"
done
# Replacement: the same identifiers transmitted a second time. A replaced
# definition must release the generation it replaced rather than accumulate.
cat "$WORK/image-1.bin" "$WORK/batch.bin" >"$WORK/replace.bin"

# A transmit declaring 4097 pixels of width — one past the frozen
# max_width_pixels — with a payload far too small to be that image.
printf '\x1b_Ga=T,f=24,s=4097,v=1,t=d;AAAA\x1b\\' >"$WORK/overflow.bin"

# ---------------------------------------------------------------------------
# Phase 0: a live server whose log is readable, and a capable viewer. Only a
# capable viewer latches a session, and only a latched session parses graphics.
# ---------------------------------------------------------------------------
scribe-test daemon stop
scribe-test server stop
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
export SCRIBE_TERMINAL_IMAGES=1
scribe-test server start
scribe-test daemon start
SESSION=$(scribe-test session create)
assert_session_alive session-start
[ -s "$SERVER_LOG" ] || fail "the live server wrote no log"

# The master switch is delivered the way an operator would deliver it: write
# the file, hot-reload, keep the session. Only a running client watches
# config.toml and this container has none.
reload_images() {
    local want="$1" mark
    printf '[terminal.images]\nenabled = %s\n' "$want" >"$CONFIG"
    mark=$(($(log_lines) + 1))
    scribe-test daemon stop
    scribe-test server upgrade
    scribe-test daemon start
    scribe-test session attach "$SESSION"
    wait_log "images_enabled=$want" "$mark" 30 ||
        fail "the server never applied terminal.images.enabled=$want"
}

# ---------------------------------------------------------------------------
# Phase 1: the text-only baseline. With the master switch off the graphics path
# is not in the session at all, which is the "before image support" side of the
# comparison US5 asks for.
# ---------------------------------------------------------------------------
reload_images false
assert_session_alive baseline-start
PID=$(server_pid) || fail "no scribe-server process to meter"
BASE_TICKS=$(cpu_ticks "$PID")
measure_text_ms baseline
BASE_TEXT_MS=$TEXT_MS
measure_input_ms baseline
BASE_CPU_TICKS=$(( $(cpu_ticks "$PID") - BASE_TICKS ))
BASE_RSS=$(status_kb "$PID" VmRSS)
echo "MEASURED baseline: ${TEXT_BYTES}B in ${BASE_TEXT_MS}ms, input median $(nth baseline 10)ms"

# ---------------------------------------------------------------------------
# Phase 2: the same session with images enabled and a resident scene. The
# capable daemon reattaching is what earns the latch back.
# ---------------------------------------------------------------------------
reload_images true
assert_session_alive images-enabled
PID=$(server_pid) || fail "no scribe-server process to meter after the switch"
RSS_BEFORE_IMAGES=$(status_kb "$PID" VmRSS)

# One image alone: PTY write to committed canonical decode.
MARK=$(($(log_lines) + 1))
TRANSFERS_BEFORE=$(log_field_max kitty_transfers)
START=$(now_ms)
send_line "cat $WORK/image-1.bin"
wait_field_at_least kitty_transfers $((TRANSFERS_BEFORE + 1)) "$MARK" 60 ||
    fail "the first measured image never decoded"
FIRST_DECODE_MS=$(($(now_ms) - START))

# The remaining fifteen back to back.
MARK=$(($(log_lines) + 1))
TRANSFERS_BEFORE=$(log_field_max kitty_transfers)
START=$(now_ms)
send_line "cat $WORK/batch.bin"
wait_field_at_least kitty_transfers $((TRANSFERS_BEFORE + IMAGE_COUNT - 1)) "$MARK" 120 ||
    fail "the fixed multi-image workload never finished decoding"
BATCH_MS=$(($(now_ms) - START))
BATCH_PER_IMAGE_MS=$((BATCH_MS / (IMAGE_COUNT - 1)))
PLACEMENTS=$(log_field_max classic_placements "$MARK")
PID=$(server_pid) || fail "no scribe-server process to meter after the workload"
RSS_AFTER_IMAGES=$(status_kb "$PID" VmRSS)
RSS_PEAK=$(status_kb "$PID" VmHWM)
RETAINED_KB=$((RSS_AFTER_IMAGES - RSS_BEFORE_IMAGES))
# What the canonical scene is worth if nothing were shared or freed: four bytes
# a pixel, once per retained definition. Recorded beside the measured resident
# delta so the review can see how the two relate.
CANONICAL_BYTES=$((IMAGE_PX * IMAGE_PX * 4 * IMAGE_COUNT))
echo "MEASURED images: first ${FIRST_DECODE_MS}ms, batch ${BATCH_PER_IMAGE_MS}ms/image, resident +${RETAINED_KB}kB"

# Replacement: the same sixteen identifiers again. Retention must not grow by
# another whole scene, because a replaced definition releases its predecessor.
MARK=$(($(log_lines) + 1))
TRANSFERS_BEFORE=$(log_field_max kitty_transfers)
START=$(now_ms)
send_line "cat $WORK/replace.bin"
wait_field_at_least kitty_transfers $((TRANSFERS_BEFORE + IMAGE_COUNT)) "$MARK" 120 ||
    fail "the replacement pass never finished decoding"
REPLACE_MS=$(($(now_ms) - START))
PID=$(server_pid) || fail "no scribe-server process to meter after replacement"
RSS_AFTER_REPLACE=$(status_kb "$PID" VmRSS)
REPLACE_GROWTH_KB=$((RSS_AFTER_REPLACE - RSS_AFTER_IMAGES))
PLACEMENTS_AFTER_REPLACE=$(log_field_max classic_placements "$MARK")

# The text measurements again, now with the scene resident and the graphics
# framer in front of every byte the session writes.
BASE_TICKS=$(cpu_ticks "$PID")
measure_text_ms images
IMAGE_TEXT_MS=$TEXT_MS
measure_input_ms images
IMAGE_CPU_TICKS=$(( $(cpu_ticks "$PID") - BASE_TICKS ))
echo "MEASURED enabled: ${TEXT_BYTES}B in ${IMAGE_TEXT_MS}ms, input median $(nth images 10)ms"

# ---------------------------------------------------------------------------
# Phase 3: the frozen ceilings are still ceilings. These are the only
# assertions in the script that compare against a number, and that number is a
# security limit rather than a performance goal.
# ---------------------------------------------------------------------------
MARK=$(($(log_lines) + 1))
FAILURES_BEFORE=$(log_field_max failures)
PLACEMENTS_BEFORE=$(log_field_max classic_placements)
send_line "cat $WORK/overflow.bin"
wait_field_at_least failures $((FAILURES_BEFORE + 1)) "$MARK" 30 ||
    fail "an over-limit declaration was accepted after the measurement workload"
[ "$(log_field_max classic_placements "$MARK")" -le "$PLACEMENTS_BEFORE" ] ||
    fail "an over-limit declaration retained a placement"
plain_log | grep -q 'panic' && fail "the server panicked during the measurement pass"
assert_session_alive ceilings-enforced
echo "MEASURED ceilings: the frozen max_width_pixels rejection still holds"

# ---------------------------------------------------------------------------
# The manifest. Payload-free by construction: durations, byte counts, and
# counters only.
# ---------------------------------------------------------------------------
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "engine": "scribe terminal image performance and resource measurement",\n'
    printf '  "surface": "linux_docker_server",\n'
    printf '  "status": "measured",\n'
    printf '  "numeric_performance_thresholds": "inapplicable",\n'
    printf '  "threshold_rationale": "clarification 7C; Constitution principle 4 permits marking numeric performance goals inapplicable while the named-command measurement requirement still applies",\n'
    printf '  "workload": {\n'
    printf '    "text_bytes": %s,\n' "$TEXT_BYTES"
    printf '    "input_samples": 20,\n'
    printf '    "image_count": %s,\n' "$IMAGE_COUNT"
    printf '    "image_pixels_per_axis": %s,\n' "$IMAGE_PX"
    printf '    "image_chunks_each": %s,\n' "${#CHUNKS[@]}"
    printf '    "canonical_rgba_bytes": %s\n' "$CANONICAL_BYTES"
    printf '  },\n'
    printf '  "text_only_baseline": {\n'
    printf '    "text_burst_ms": %s,\n' "$BASE_TEXT_MS"
    printf '    "input_ms_min": %s,\n' "$(nth baseline 1)"
    printf '    "input_ms_median": %s,\n' "$(nth baseline 10)"
    printf '    "input_ms_max": %s,\n' "$(nth baseline 20)"
    printf '    "server_cpu_ms": %s,\n' "$((BASE_CPU_TICKS * 1000 / TICKS_PER_SEC))"
    printf '    "server_rss_kb": %s\n' "$BASE_RSS"
    printf '  },\n'
    printf '  "images_enabled": {\n'
    printf '    "text_burst_ms": %s,\n' "$IMAGE_TEXT_MS"
    printf '    "input_ms_min": %s,\n' "$(nth images 1)"
    printf '    "input_ms_median": %s,\n' "$(nth images 10)"
    printf '    "input_ms_max": %s,\n' "$(nth images 20)"
    printf '    "server_cpu_ms": %s,\n' "$((IMAGE_CPU_TICKS * 1000 / TICKS_PER_SEC))"
    printf '    "server_rss_kb": %s\n' "$RSS_AFTER_REPLACE"
    printf '  },\n'
    printf '  "decode": {\n'
    printf '    "first_image_ms": %s,\n' "$FIRST_DECODE_MS"
    printf '    "batch_ms": %s,\n' "$BATCH_MS"
    printf '    "batch_ms_per_image": %s,\n' "$BATCH_PER_IMAGE_MS"
    printf '    "replacement_ms": %s\n' "$REPLACE_MS"
    printf '  },\n'
    printf '  "retention": {\n'
    printf '    "server_rss_kb_before_images": %s,\n' "$RSS_BEFORE_IMAGES"
    printf '    "server_rss_kb_after_images": %s,\n' "$RSS_AFTER_IMAGES"
    printf '    "server_rss_kb_delta": %s,\n' "$RETAINED_KB"
    printf '    "server_rss_kb_peak": %s,\n' "$RSS_PEAK"
    printf '    "replacement_rss_kb_growth": %s,\n' "$REPLACE_GROWTH_KB"
    printf '    "placements_after_workload": %s,\n' "$PLACEMENTS"
    printf '    "placements_after_replacement": %s,\n' "$PLACEMENTS_AFTER_REPLACE"
    printf '    "evictions_required": false\n'
    printf '  },\n'
    printf '  "security_ceilings": {\n'
    printf '    "max_width_pixels_rejected_after_load": true,\n'
    printf '    "retained_placement_on_rejection": false,\n'
    printf '    "server_panicked": false\n'
    printf '  },\n'
    printf '  "counters": {\n'
    printf '    "kitty_commands": %s,\n' "$(log_field_max kitty_commands)"
    printf '    "kitty_transfers": %s,\n' "$(log_field_max kitty_transfers)"
    printf '    "failures": %s\n' "$(log_field_max failures)"
    printf '  }\n'
    printf '}\n'
} >"$EVIDENCE"

grep -Eq '"[A-Za-z0-9_]+": \[' "$EVIDENCE" && fail "the manifest embedded array-shaped data"
echo "PASS: terminal image performance and resource measurements recorded in $EVIDENCE"
