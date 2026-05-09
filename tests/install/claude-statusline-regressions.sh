#!/bin/bash
# Offline regression tests for dist/scribe-claude-statusline.sh.
#
# Each test feeds a synthetic CC statusLine JSON through the script,
# captures both stdout and the OSC bytes that the script writes to
# /dev/tty via `script(1)`, and asserts expected content.

set -euo pipefail

if [[ "$(uname)" == "Darwin" ]]; then
    echo "claude-statusline-regressions: skip on macOS (script(1) signature differs)"
    exit 0
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
statusline="${repo_root}/dist/scribe-claude-statusline.sh"

if [ ! -x "$statusline" ]; then
    echo "FAIL: scribe-claude-statusline.sh not found or not executable at ${statusline}" >&2
    exit 2
fi

failures=0
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# capture_run <json>
#   Runs the statusline script with the given JSON on stdin under script(1)
#   so that both stdout and /dev/tty writes are captured into a single file.
#   Sets global CAPTURE_FILE to the path of the captured output.
CAPTURE_FILE=""
capture_run() {
    local json="$1"
    CAPTURE_FILE="${tmp_dir}/capture_$$.txt"
    rm -f "$CAPTURE_FILE"
    # Use printf to avoid subshell quoting issues with complex JSON.
    # script -q -c '<cmd>' <logfile> captures the pseudo-tty output (which
    # includes both stdout and anything written to /dev/tty) into <logfile>.
    local escaped
    escaped="$(printf '%s' "$json" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))')"
    script -q -c "printf %s ${escaped} | ${statusline}" "$CAPTURE_FILE" >/dev/null 2>&1 || true
}

# ── case_used_percentage_present ─────────────────────────────────────────────
case_name="case_used_percentage_present"
capture_run '{"context_window":{"used_percentage":73},"model":{"display_name":"Sonnet 4.6"}}'
if grep -qaF $'\x1b]1337;ClaudeContext=73\x07' "$CAPTURE_FILE" \
   && grep -qaF 'Sonnet 4.6 • 73%' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=73 and banner 'Sonnet 4.6 • 73%' in captured output"
    failures=$((failures + 1))
fi

# ── case_fallback_to_tokens ───────────────────────────────────────────────────
# 50000 + 10000 = 60000 / 200000 = 30%
case_name="case_fallback_to_tokens"
capture_run '{"context_window":{"context_window_size":200000,"total_input_tokens":50000,"total_output_tokens":10000},"model":{"display_name":"Opus"}}'
if grep -qaF $'\x1b]1337;ClaudeContext=30\x07' "$CAPTURE_FILE" \
   && grep -qaF 'Opus • 30%' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=30 and banner 'Opus • 30%' in captured output"
    failures=$((failures + 1))
fi

# ── case_missing_context_window ───────────────────────────────────────────────
case_name="case_missing_context_window"
capture_run '{"model":{"display_name":"Opus"}}'
if ! grep -qaF $'\x1b]1337;ClaudeContext=' "$CAPTURE_FILE" \
   && grep -qaF 'Opus' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected no OSC context= and banner with 'Opus' in captured output"
    failures=$((failures + 1))
fi

# ── case_malformed_json ───────────────────────────────────────────────────────
case_name="case_malformed_json"
capture_run 'not-json'
if ! grep -qaF $'\x1b]1337;ClaudeContext=' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected no OSC emission for malformed JSON"
    failures=$((failures + 1))
fi

# ── case_clamp_overflow ───────────────────────────────────────────────────────
case_name="case_clamp_overflow"
capture_run '{"context_window":{"used_percentage":150},"model":{"display_name":"Opus"}}'
if grep -qaF $'\x1b]1337;ClaudeContext=100\x07' "$CAPTURE_FILE" \
   && grep -qaF 'Opus • 100%' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=100 (clamped from 150) and banner 'Opus • 100%'"
    failures=$((failures + 1))
fi

# ── case_clamp_underflow ──────────────────────────────────────────────────────
case_name="case_clamp_underflow"
capture_run '{"context_window":{"used_percentage":-5},"model":{"display_name":"Opus"}}'
if grep -qaF $'\x1b]1337;ClaudeContext=0\x07' "$CAPTURE_FILE" \
   && grep -qaF 'Opus • 0%' "$CAPTURE_FILE"; then
    echo "PASS: ${case_name}"
else
    echo "FAIL: ${case_name}"
    echo "  expected OSC context=0 (clamped from -5) and banner 'Opus • 0%'"
    failures=$((failures + 1))
fi

# ── Summary ───────────────────────────────────────────────────────────────────
if [ "$failures" -gt 0 ]; then
    echo "${failures} claude-statusline regression test(s) failed."
    exit 1
fi

echo "claude-statusline-regressions: all cases passed"
