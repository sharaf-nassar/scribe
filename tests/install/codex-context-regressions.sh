#!/bin/bash
# Offline regression tests for dist/detect-codex-context.sh.
#
# Each test seeds a synthetic ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
# in a fresh temp HOME, runs the producer via script(1) to capture /dev/tty
# writes, and asserts the emitted OSC bytes carry the expected percentage.

set -euo pipefail

if [[ "$(uname)" == "Darwin" ]]; then
    echo "codex-context-regressions: skip on macOS (script(1) signature differs)"
    exit 0
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
producer="${repo_root}/dist/detect-codex-context.sh"

if [ ! -x "$producer" ]; then
    echo "FAIL: detect-codex-context.sh not found or not executable at ${producer}" >&2
    exit 2
fi

failures=0
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# capture_run <home_dir>
#   Runs the producer with HOME set to <home_dir> under script(1) so that
#   both stdout and anything written to /dev/tty are captured to CAPTURE_FILE.
#   Feeds empty JSON on stdin (Codex hook contract — stdin must be drained).
CAPTURE_FILE=""
capture_run() {
    local home_dir="$1"
    CAPTURE_FILE="${tmp_dir}/capture_$$.txt"
    rm -f "$CAPTURE_FILE"
    # script(1) allocates a pty, runs the command, and saves all terminal
    # output (including OSC sequences written to /dev/tty) to the log file.
    script -q -c "echo '{}' | HOME='${home_dir}' '${producer}'" "$CAPTURE_FILE" >/dev/null 2>&1 || true
}

# ── case_total_token_usage_present ────────────────────────────────────────────
# 52000 / 260000 = 0.20 → context=20
case_name="case_total_token_usage_present"
home1="${tmp_dir}/home1"
mkdir -p "${home1}/.codex/sessions/2026/05/06"
cat > "${home1}/.codex/sessions/2026/05/06/rollout-test.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"output_tokens":2000,"total_tokens":52000},"last_token_usage":{"input_tokens":50000,"output_tokens":2000,"total_tokens":52000},"model_context_window":260000}}}
EOF
capture_run "$home1"
if grep -qaF $'\x1b]1337;CodexState=processing;context=20\x07' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=20 (52000/260000) in captured output"
    failures=$((failures + 1))
fi

# ── case_last_token_usage_fallback ────────────────────────────────────────────
# total_token_usage absent; last_token_usage.total_tokens=100000 / 200000 = 0.50 → context=50
case_name="case_last_token_usage_fallback"
home2="${tmp_dir}/home2"
mkdir -p "${home2}/.codex/sessions/2026/05/06"
cat > "${home2}/.codex/sessions/2026/05/06/rollout-test.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":100000},"model_context_window":200000}}}
EOF
capture_run "$home2"
if grep -qaF $'\x1b]1337;CodexState=processing;context=50\x07' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=50 (100000/200000 via last_token_usage fallback)"
    failures=$((failures + 1))
fi

# ── case_default_window_when_absent ───────────────────────────────────────────
# model_context_window absent → _default=200000; 100000/200000 = 0.50 → context=50
case_name="case_default_window_when_absent"
home3="${tmp_dir}/home3"
mkdir -p "${home3}/.codex/sessions/2026/05/06"
cat > "${home3}/.codex/sessions/2026/05/06/rollout-test.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100000}}}}
EOF
capture_run "$home3"
if grep -qaF $'\x1b]1337;CodexState=processing;context=50\x07' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=50 (100000 / _default 200000)"
    failures=$((failures + 1))
fi

# ── case_clamp_overflow ───────────────────────────────────────────────────────
# 9000000 / 258400 >> 100 → clamped to context=100
case_name="case_clamp_overflow"
home4="${tmp_dir}/home4"
mkdir -p "${home4}/.codex/sessions/2026/05/06"
cat > "${home4}/.codex/sessions/2026/05/06/rollout-test.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":9000000},"model_context_window":258400}}}
EOF
capture_run "$home4"
if grep -qaF $'\x1b]1337;CodexState=processing;context=100\x07' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=100 (clamped from 9000000/258400)"
    failures=$((failures + 1))
fi

# ── case_no_token_count_record ────────────────────────────────────────────────
# Only a task_started record — no token usage → no OSC emitted, exit 0
case_name="case_no_token_count_record"
home5="${tmp_dir}/home5"
mkdir -p "${home5}/.codex/sessions/2026/05/06"
cat > "${home5}/.codex/sessions/2026/05/06/rollout-test.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"event_msg","payload":{"type":"task_started","model_context_window":258400}}
EOF
capture_run "$home5"
if ! grep -qaF $'\x1b]1337;CodexState=processing;context=' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected no OSC CodexState=processing;context= for task_started-only rollout"
    failures=$((failures + 1))
fi

# ── case_empty_sessions_dir ───────────────────────────────────────────────────
# sessions dir exists but contains no .jsonl files → no OSC, exit 0
case_name="case_empty_sessions_dir"
home6="${tmp_dir}/home6"
mkdir -p "${home6}/.codex/sessions"
capture_run "$home6"
if ! grep -qaF $'\x1b]1337;CodexState=processing;context=' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected no OSC CodexState=processing;context= for empty sessions dir"
    failures=$((failures + 1))
fi

# ── Summary ───────────────────────────────────────────────────────────────────
if [ "$failures" -gt 0 ]; then
    echo "${failures} codex-context regression test(s) failed."
    exit 1
fi

echo "codex-context-regressions: all cases passed"
