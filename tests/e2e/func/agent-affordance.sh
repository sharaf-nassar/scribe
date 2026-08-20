#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests]]
set -euo pipefail

PREFIX=/usr/local/share/scribe
CLAUDE_SETUP="$PREFIX/setup-claude-hooks.sh"
CODEX_SETUP="$PREFIX/setup-codex-hooks.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ -x "$CLAUDE_SETUP" ] || fail "Claude setup script is absent from the functional image"
[ -x "$CODEX_SETUP" ] || fail "Codex setup script is absent from the functional image"
command -v scribe >/dev/null 2>&1 || fail "scribe CLI is absent from the functional image"

mkdir -p "$HOME/.claude" "$HOME/.codex"
printf '{}\n' >"$HOME/.claude/settings.json"
printf '' >"$HOME/.codex/config.toml"

run_setup() {
    local provider="$1" stdout="$2" stderr="$3"
    case "$provider" in
        claude)
            SCRIBE_INSTALL_PREFIX="$PREFIX" "$CLAUDE_SETUP" >"$stdout" 2>"$stderr"
            ;;
        codex)
            SCRIBE_INSTALL_PREFIX="$PREFIX" "$CODEX_SETUP" \
                --hook-source "$PREFIX" >"$stdout" 2>"$stderr"
            ;;
        *) fail "unknown provider $provider" ;;
    esac
}

check_provider() {
    local provider="$1" skill="$2"
    run_setup "$provider" "/output/agent-affordance-$provider-first.out" \
        "/output/agent-affordance-$provider-first.err"
    [ -f "$skill" ] || fail "$provider setup did not create $skill"
    grep -q 'SCRIBE-MANAGED-SKILL' "$skill" \
        || fail "$provider skill lacks the Scribe ownership marker"
    grep -q '# Scribe agent control' "$skill" \
        || fail "$provider skill lacks generated CLI guidance"
    grep -q 'scribe agent siblings' "$skill" \
        || fail "$provider skill omitted the siblings command"
    grep -q 'unavailable; enable `agent_api.read_content`' "$skill" \
        || fail "$provider skill did not reflect the default-deny live policy"

    local before after
    before="$(stat -c '%i:%Y:%s' "$skill"):$(sha256sum "$skill" | cut -d' ' -f1)"
    sleep 1
    run_setup "$provider" "/output/agent-affordance-$provider-second.out" \
        "/output/agent-affordance-$provider-second.err"
    after="$(stat -c '%i:%Y:%s' "$skill"):$(sha256sum "$skill" | cut -d' ' -f1)"
    [ "$before" = "$after" ] \
        || fail "$provider regenerated an unchanged owned skill ($before -> $after)"
    grep -q 'already up to date' "/output/agent-affordance-$provider-second.out" \
        || fail "$provider did not report the idempotent no-op"
    echo "PHASE $provider-1 PASS: generated affordance regeneration was idempotent"

    printf 'foreign-%s-skill\n' "$provider" >"$skill"
    local foreign_before foreign_after
    foreign_before=$(sha256sum "$skill" | cut -d' ' -f1)
    run_setup "$provider" "/output/agent-affordance-$provider-foreign.out" \
        "/output/agent-affordance-$provider-foreign.err"
    foreign_after=$(sha256sum "$skill" | cut -d' ' -f1)
    [ "$foreign_before" = "$foreign_after" ] \
        || fail "$provider clobbered a foreign skill file"
    grep -q 'was not installed by Scribe; leaving it untouched' \
        "/output/agent-affordance-$provider-foreign.err" \
        || fail "$provider foreign-file refusal was not reported"
    echo "PHASE $provider-2 PASS: foreign affordance file was refused unchanged"
}

check_provider claude "$HOME/.claude/skills/scribe-terminal/SKILL.md"
check_provider codex "$HOME/.codex/skills/scribe-terminal/SKILL.md"

echo "PASS: generated agent affordance install coverage completed"
