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
    bd create 'Status fixture' --id write-status --type task --priority 2 >/dev/null
    bd create 'Guard race fixture' --id write-race --type task --priority 2 >/dev/null
    bd create 'Failure fixture' --id write-fault --type task --priority 2 >/dev/null
)

scribe-test daemon stop
scribe-test server stop
printf '[workspaces]\nroots = ["%s"]\n' "$ROOT" >"$HOME/.config/scribe/config.toml"
export BD_NO_DAEMON=1
FAULT_BIN="$ROOT/fault-bin"
mkdir -p "$FAULT_BIN"
ln -s /tests/fixtures/bd-write-fault.sh "$FAULT_BIN/bd"
rm -f /tmp/scribe-beads-write-fault-mode
PATH="$FAULT_BIN:$PATH" scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create --cwd "$PROJECT")
scribe-test wait-cwd "$SESSION" "$PROJECT"
BASELINE_BOARD=$(scribe-test beads-board)
printf '%s\n' "$BASELINE_BOARD" | grep -Fq '"id":"write-fault","title":"Failure fixture"' \
    || fail "failure fixture was absent from last-good board: $BASELINE_BOARD"

write_applied() {
    local issue_id="$1" verb="$2" status="$3" assignee="$4"
    shift 4
    local result
    result=$(scribe-test beads-write "$issue_id" "$verb" "$@" \
        --if-status "$status" --if-assignee "$assignee")
    printf '%s\n' "$result" | grep -Fq '"applied"' \
        || fail "$verb was not applied: $result"
    printf '%s\n' "$result" | grep -Fq '"board_pushed":true' \
        || fail "$verb did not push a board refresh: $result"
}

write_applied write-target set-title open '' --value 'Persisted through Scribe'
write_applied write-target set-description open '' --value 'Persisted description'
write_applied write-target set-acceptance open '' --value 'Persisted acceptance'
write_applied write-target set-notes open '' --value 'Persisted notes'
write_applied write-target set-design open '' --value 'Persisted design'
write_applied write-target set-spec-id open '' --value '024-write-persistence'
write_applied write-target set-priority open '' --value 1
write_applied write-target set-type open '' --value feature
write_applied write-target set-labels open '' --value 'client,persistence'
write_applied write-target add-comment open '' --value 'Guarded through Scribe'

DETAIL=$(cd "$PROJECT" && bd show write-target --json --include-comments)
for expected in \
    '"title": "Persisted through Scribe"' \
    '"description": "Persisted description"' \
    '"acceptance_criteria": "Persisted acceptance"' \
    '"notes": "Persisted notes"' \
    '"design": "Persisted design"' \
    '"spec_id": "024-write-persistence"' \
    '"priority": 1' \
    '"issue_type": "feature"' \
    '"client"' \
    '"persistence"' \
    '"text": "Guarded through Scribe"'
do
    printf '%s\n' "$DETAIL" | grep -Fq "$expected" \
        || fail "persisted field family omitted $expected: $DETAIL"
done
printf '%s\n' "$DETAIL" >/output/beads-write-fields.json

write_applied write-status set-status open '' --value in_progress
STATUS=$(cd "$PROJECT" && bd show write-status --json)
printf '%s\n' "$STATUS" | grep -Fq '"status": "in_progress"' \
    || fail "status write was not persisted: $STATUS"

write_applied write-target claim open ''
CLAIMED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$CLAIMED" | grep -Fq '"status": "in_progress"' \
    || fail "claim did not persist in_progress: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Fq '"assignee": "Scribe Write Actor"' \
    || fail "claim did not use bd actor resolution: $CLAIMED"

# Capture open/unassigned guards, then mutate through real bd before Scribe
# uses them. The stale comment must become the typed precondition result.
(cd "$PROJECT" && bd update write-race --status in_progress >/dev/null)
RACE_RESULT=$(scribe-test beads-write write-race add-comment \
    --value 'must not land' --if-status open --if-assignee '')
printf '%s\n' "$RACE_RESULT" | grep -Fq '"precondition_failed"' \
    || fail "seeded guard race did not map to typed precondition failure: $RACE_RESULT"
RACE=$(cd "$PROJECT" && bd show write-race --json --include-comments)
printf '%s\n' "$RACE" | grep -Fq '"status": "in_progress"' \
    || fail "guard race lost the winning status: $RACE"
if printf '%s\n' "$RACE" | grep -Fq 'must not land'; then
    fail "guard race persisted the losing comment: $RACE"
fi

CLOSE_AT=$SECONDS
write_applied write-target close in_progress 'Scribe Write Actor'
CLOSED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "close was not persisted: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Eq '"closed_at": "[0-9]{4}-' \
    || fail "close omitted native closed_at: $CLOSED"
[ "$((SECONDS - CLOSE_AT))" -lt 5 ] \
    || fail "close result exceeded the five-second undo window"
write_applied write-target undo-close closed 'Scribe Write Actor'
REOPENED=$(cd "$PROJECT" && bd show write-target --json)
printf '%s\n' "$REOPENED" | grep -Fq '"status": "open"' \
    || fail "undo was not persisted: $REOPENED"
if printf '%s\n' "$REOPENED" | grep -Eq '"closed_at": "'; then
    fail "undo retained closed_at: $REOPENED"
fi

printf '%s\n' nonzero >/tmp/scribe-beads-write-fault-mode
NONZERO_RESULT=$(scribe-test beads-write write-fault set-title \
    --value 'must not persist' --if-status open --if-assignee '')
rm -f /tmp/scribe-beads-write-fault-mode
printf '%s\n' "$NONZERO_RESULT" | grep -Fq '"failed"' \
    || fail "nonzero write did not return Failed: $NONZERO_RESULT"
printf '%s\n' "$NONZERO_RESULT" | grep -Fq 'forced nonzero write' \
    || fail "nonzero write lost bd error detail: $NONZERO_RESULT"
printf '%s\n' "$NONZERO_RESULT" | grep -Fq '"board_pushed":false' \
    || fail "nonzero write pushed a replacement board: $NONZERO_RESULT"
NONZERO_SHOW=$(cd "$PROJECT" && bd show write-fault --json)
printf '%s\n' "$NONZERO_SHOW" | grep -Fq '"title": "Failure fixture"' \
    || fail "nonzero write changed persisted state: $NONZERO_SHOW"

printf '%s\n' timeout >/tmp/scribe-beads-write-fault-mode
TIMEOUT_RESULT=$(scribe-test beads-write write-fault set-title \
    --value 'must not time in' --if-status open --if-assignee '')
rm -f /tmp/scribe-beads-write-fault-mode
printf '%s\n' "$TIMEOUT_RESULT" | grep -Fq '"failed"' \
    || fail "timeout write did not return Failed: $TIMEOUT_RESULT"
printf '%s\n' "$TIMEOUT_RESULT" | grep -Fq 'bd issue write timed out' \
    || fail "timeout write lost deadline reason: $TIMEOUT_RESULT"
printf '%s\n' "$TIMEOUT_RESULT" | grep -Fq '"board_pushed":false' \
    || fail "timeout write pushed a replacement board: $TIMEOUT_RESULT"
TIMEOUT_SHOW=$(cd "$PROJECT" && bd show write-fault --json)
printf '%s\n' "$TIMEOUT_SHOW" | grep -Fq '"title": "Failure fixture"' \
    || fail "timeout write changed persisted state: $TIMEOUT_SHOW"
AFTER_FAILURES=$(scribe-test beads-board)
printf '%s\n' "$AFTER_FAILURES" | grep -Fq '"id":"write-fault","title":"Failure fixture"' \
    || fail "failure replaced the last-good board: $AFTER_FAILURES"

printf '%s\n' "$TIMEOUT_SHOW" >/output/beads-write-final-show.json
printf '%s\n' "$AFTER_FAILURES" >/output/beads-write-last-good.json

echo 'PASS: every write family persisted; races and failures preserved authoritative state'
