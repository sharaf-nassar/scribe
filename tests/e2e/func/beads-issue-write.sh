#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Server Beads Issue Writes]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

ROOT=/tmp/scribe-beads-write-root
PROJECT="$ROOT/project"
mkdir -p "$PROJECT"
git -C "$PROJECT" init --quiet
git -C "$PROJECT" config user.email write@example.invalid
git -C "$PROJECT" config user.name 'Scribe Write Actor'
(
    cd "$PROJECT"
    bd init --quiet --stealth --prefix write
    bd create 'Server write fixture' --id write-target --type task --priority 2 >/dev/null
)

scribe-test daemon stop
scribe-test server stop
printf '[workspaces]\nroots = ["%s"]\n' "$ROOT" >"$HOME/.config/scribe/config.toml"
export BD_NO_DAEMON=1
scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create --cwd "$PROJECT")
scribe-test wait-cwd "$SESSION" "$PROJECT"
scribe-test beads-board >/dev/null

TITLE_RESULT=$(scribe-test beads-write write-target set-title \
    --value 'Persisted through Scribe' --if-status open --if-assignee '')
printf '%s\n' "$TITLE_RESULT" | grep -Fq '"applied"' \
    || fail "title write was not applied: $TITLE_RESULT"
printf '%s\n' "$TITLE_RESULT" | grep -Fq '"board_pushed":true' \
    || fail "title write did not push a board refresh: $TITLE_RESULT"
DETAIL=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$DETAIL" | grep -Fq '"title": "Persisted through Scribe"' \
    || fail "title write was not persisted: $DETAIL"

COMMENT_RESULT=$(scribe-test beads-write write-target add-comment \
    --value 'Guarded through Scribe' --if-status open --if-assignee '')
printf '%s\n' "$COMMENT_RESULT" | grep -Fq '"applied"' \
    || fail "guarded comment was not applied: $COMMENT_RESULT"

CLAIM_RESULT=$(scribe-test beads-write write-target claim \
    --if-status open --if-assignee '')
printf '%s\n' "$CLAIM_RESULT" | grep -Fq '"applied"' \
    || fail "guarded claim was not applied: $CLAIM_RESULT"
CLAIMED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$CLAIMED" | grep -Fq '"assignee": "Scribe Write Actor"' \
    || fail "claim did not use bd actor resolution: $CLAIMED"

STALE_COMMENT=$(scribe-test beads-write write-target add-comment \
    --value 'must not land' --if-status open --if-assignee '')
printf '%s\n' "$STALE_COMMENT" | grep -Fq '"precondition_failed"' \
    || fail "stale comment did not map rc13: $STALE_COMMENT"
COMMENTS=$(cd "$PROJECT" && bd show write-target --json --include-comments)
printf '%s\n' "$COMMENTS" | grep -Fq '"text": "Guarded through Scribe"' \
    || fail "guarded comment was not persisted: $COMMENTS"
if printf '%s\n' "$COMMENTS" | grep -Fq 'must not land'; then
    fail "stale guarded comment changed persisted state: $COMMENTS"
fi

STALE=$(scribe-test beads-write write-target close \
    --if-status open --if-assignee 'Scribe Write Actor')
printf '%s\n' "$STALE" | grep -Fq '"precondition_failed"' \
    || fail "stale close did not map rc13: $STALE"
STILL_OPEN=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$STILL_OPEN" | grep -Fq '"status": "in_progress"' \
    || fail "stale close changed persisted state: $STILL_OPEN"

CLOSE_RESULT=$(scribe-test beads-write write-target close \
    --if-status in_progress --if-assignee 'Scribe Write Actor')
printf '%s\n' "$CLOSE_RESULT" | grep -Fq '"applied"' \
    || fail "guarded close was not applied: $CLOSE_RESULT"
CLOSED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "close was not persisted: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Eq '"closed_at": "[0-9]{4}-' \
    || fail "close omitted native closed_at: $CLOSED"

UNDO_RESULT=$(scribe-test beads-write write-target undo-close \
    --if-status closed --if-assignee 'Scribe Write Actor')
printf '%s\n' "$UNDO_RESULT" | grep -Fq '"applied"' \
    || fail "guarded undo was not applied: $UNDO_RESULT"
REOPENED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$REOPENED" | grep -Fq '"status": "open"' \
    || fail "undo was not persisted: $REOPENED"
if printf '%s\n' "$REOPENED" | grep -Eq '"closed_at": "'; then
    fail "undo retained closed_at: $REOPENED"
fi

echo 'PASS: server writes persist, guard conflicts preserve state, and applied writes push boards'
