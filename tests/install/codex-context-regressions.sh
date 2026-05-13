#!/bin/bash
# Offline regression tests for dist/detect-codex-context.sh.
#
# Each test seeds a synthetic Codex rollout JSONL, passes its transcript_path
# through hook stdin, and asserts the emitted OSC bytes carry the expected
# percentage. The producer intentionally has no global newest-file fallback.

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

CAPTURE_FILE=""
capture_run() {
    local home_dir="$1"
    local payload="${2-}"
    local payload_file="${tmp_dir}/payload_$$.json"
    if [[ -z "$payload" ]]; then
        payload="{}"
    fi
    CAPTURE_FILE="${tmp_dir}/capture_$$.txt"
    printf '%s' "$payload" > "$payload_file"
    rm -f "$CAPTURE_FILE"
    script -q -c "HOME='${home_dir}' '${producer}' < '${payload_file}'" "$CAPTURE_FILE" >/dev/null 2>&1 || true
}

assert_context() {
    local case_name="$1"
    local expected="$2"
    if grep -qaF $'\x1b]1337;CodexContext='"${expected}"$'\x07' "$CAPTURE_FILE"; then
        echo "PASS: ${case_name}"
    else
        echo "FAIL: ${case_name}"
        echo "  expected OSC context=${expected} in captured output"
        failures=$((failures + 1))
    fi
}

assert_no_context() {
    local case_name="$1"
    local reason="$2"
    if ! grep -qaF $'\x1b]1337;CodexContext=' "$CAPTURE_FILE"; then
        echo "PASS: ${case_name}"
    else
        echo "FAIL: ${case_name}"
        echo "  expected no OSC CodexContext= for ${reason}"
        failures=$((failures + 1))
    fi
}

payload_for() {
    local transcript_path="$1"
    printf '{"session_id":"target-session","transcript_path":"%s","cwd":"/tmp/scribe-target"}' "$transcript_path"
}

# ── case_last_token_usage_present ────────────────────────────────────────────
# 52000 / 260000 = 0.20 → context=20
case_name="case_last_token_usage_present"
home1="${tmp_dir}/home1"
mkdir -p "${home1}/.codex/sessions/2026/05/06"
target1="${home1}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
cat > "$target1" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":999999},"last_token_usage":{"input_tokens":50000,"output_tokens":2000,"total_tokens":52000},"model_context_window":260000}}}
EOF
capture_run "$home1" "$(payload_for "$target1")"
assert_context "$case_name" "20"

# ── case_transcript_path_selects_matching_rollout ────────────────────────────
# An unrelated newer rollout must not leak its higher context into this hook.
case_name="case_transcript_path_selects_matching_rollout"
home2="${tmp_dir}/home2"
mkdir -p "${home2}/.codex/sessions/2026/05/06"
target2="${home2}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
unrelated2="${home2}/.codex/sessions/2026/05/06/rollout-unrelated-session.jsonl"
cat > "$target2" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":50000},"total_token_usage":{"total_tokens":50000},"model_context_window":200000}}}
EOF
cat > "$unrelated2" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"unrelated-session","cwd":"/tmp/other-project"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":160000},"total_token_usage":{"total_tokens":160000},"model_context_window":200000}}}
EOF
touch -d '2026-05-06 01:00:00 UTC' "$target2"
touch -d '2026-05-06 01:00:05 UTC' "$unrelated2"
capture_run "$home2" "$(payload_for "$target2")"
assert_context "$case_name" "25"

# ── case_clamp_overflow ───────────────────────────────────────────────────────
# 9000000 / 258400 >> 100 → clamped to context=100
case_name="case_clamp_overflow"
home3="${tmp_dir}/home3"
mkdir -p "${home3}/.codex/sessions/2026/05/06"
target3="${home3}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
cat > "$target3" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":9000000},"total_token_usage":{"total_tokens":9000000},"model_context_window":258400}}}
EOF
capture_run "$home3" "$(payload_for "$target3")"
assert_context "$case_name" "100"

# ── case_no_transcript_path ───────────────────────────────────────────────────
case_name="case_no_transcript_path"
home4="${tmp_dir}/home4"
mkdir -p "${home4}/.codex/sessions/2026/05/06"
cat > "${home4}/.codex/sessions/2026/05/06/rollout-target-session.jsonl" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":50000},"model_context_window":200000}}}
EOF
capture_run "$home4" '{"session_id":"target-session","cwd":"/tmp/scribe-target"}'
assert_no_context "$case_name" "missing transcript_path"

# ── case_invalid_transcript_path ──────────────────────────────────────────────
case_name="case_invalid_transcript_path"
home5="${tmp_dir}/home5"
mkdir -p "${home5}/.codex/sessions"
capture_run "$home5" '{"session_id":"target-session","transcript_path":"/tmp/not-a-codex-rollout.jsonl","cwd":"/tmp/scribe-target"}'
assert_no_context "$case_name" "invalid transcript_path"

# ── case_no_token_count_record ────────────────────────────────────────────────
case_name="case_no_token_count_record"
home6="${tmp_dir}/home6"
mkdir -p "${home6}/.codex/sessions/2026/05/06"
target6="${home6}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
cat > "$target6" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"task_started","model_context_window":258400}}
EOF
capture_run "$home6" "$(payload_for "$target6")"
assert_no_context "$case_name" "task_started-only rollout"

# ── case_missing_last_token_usage ─────────────────────────────────────────────
case_name="case_missing_last_token_usage"
home7="${tmp_dir}/home7"
mkdir -p "${home7}/.codex/sessions/2026/05/06"
target7="${home7}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
cat > "$target7" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100000},"model_context_window":200000}}}
EOF
capture_run "$home7" "$(payload_for "$target7")"
assert_no_context "$case_name" "missing last_token_usage"

# ── case_missing_context_window ───────────────────────────────────────────────
case_name="case_missing_context_window"
home8="${tmp_dir}/home8"
mkdir -p "${home8}/.codex/sessions/2026/05/06"
target8="${home8}/.codex/sessions/2026/05/06/rollout-target-session.jsonl"
cat > "$target8" <<'EOF'
{"timestamp":"2026-05-06T01:00:00.000Z","type":"session_meta","payload":{"id":"target-session","cwd":"/tmp/scribe-target"}}
{"timestamp":"2026-05-06T01:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":100000}}}}
EOF
capture_run "$home8" "$(payload_for "$target8")"
assert_no_context "$case_name" "missing model_context_window"

if [ "$failures" -gt 0 ]; then
    echo "${failures} codex-context regression test(s) failed."
    exit 1
fi

echo "codex-context-regressions: all cases passed"
