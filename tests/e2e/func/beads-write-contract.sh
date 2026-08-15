#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Guarded Beads Write Contract]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

ROOT=/tmp/scribe-beads-write-contract
PROJECT="$ROOT/project"
mkdir -p "$PROJECT"
git -C "$PROJECT" init --quiet
git -C "$PROJECT" config user.email contract@example.invalid
git -C "$PROJECT" config user.name 'Scribe Contract Actor'

VERSION=$(bd version)
printf '%s\n' "$VERSION" | grep -Fq 'scribe-guards-7505e173f265' \
    || fail "unexpected bd build: $VERSION"

for command in update close reopen; do
    bd "$command" --help | grep -Fq -- '--if-status' \
        || fail "$command omits --if-status"
    bd "$command" --help | grep -Fq -- '--if-assignee' \
        || fail "$command omits --if-assignee"
done
bd comments add --help | grep -Fq -- '--if-status' \
    || fail "comments add omits --if-status"
bd comments add --help | grep -Fq -- '--if-assignee' \
    || fail "comments add omits --if-assignee"

cd "$PROJECT"
bd init --quiet --stealth --prefix contract
bd create 'Guarded issue' --id contract-guard --type task --priority 2 >/dev/null
bd create 'Status issue' --id contract-status --type task --priority 3 >/dev/null

bd update contract-guard --title 'Guarded field update' \
    --if-status open --if-assignee '' --json >/dev/null
FIELD=$(bd show contract-guard --json)
printf '%s\n' "$FIELD" | grep -Fq '"title": "Guarded field update"' \
    || fail "guarded field update did not commit: $FIELD"

bd update contract-guard --add-label atomic-label \
    --if-status open --if-assignee '' --json >/dev/null
LABELLED=$(bd show contract-guard --json)
printf '%s\n' "$LABELLED" | grep -Fq '"atomic-label"' \
    || fail "guarded label-only update did not commit: $LABELLED"

bd comments add contract-guard 'atomic comment' \
    --if-status open --if-assignee '' --json >/dev/null
set +e
bd comments add contract-guard 'must not land' \
    --if-status closed --if-assignee '' --json \
    >"$ROOT/comment-mismatch.out" 2>"$ROOT/comment-mismatch.err"
COMMENT_MISMATCH_RC=$?
set -e
[ "$COMMENT_MISMATCH_RC" -eq 13 ] \
    || fail "comment mismatch returned rc $COMMENT_MISMATCH_RC instead of 13"
grep -Fq '"guard_mismatch":true' "$ROOT/comment-mismatch.err" \
    || fail "comment mismatch omitted structured JSON: $(cat "$ROOT/comment-mismatch.err")"
COMMENTS=$(bd show contract-guard --json --include-comments)
printf '%s\n' "$COMMENTS" | grep -Fq '"text": "atomic comment"' \
    || fail "guarded comment did not commit: $COMMENTS"
if printf '%s\n' "$COMMENTS" | grep -Fq 'must not land'; then
    fail "stale guarded comment changed state: $COMMENTS"
fi

bd update contract-status --status in_progress \
    --if-status open --if-assignee '' --json >/dev/null
STATUS=$(bd show contract-status --json)
printf '%s\n' "$STATUS" | grep -Fq '"status": "in_progress"' \
    || fail "guarded status update did not commit: $STATUS"

set +e
bd update contract-guard --title 'must not commit' \
    --if-status closed --if-assignee '' --json \
    >"$ROOT/mismatch.out" 2>"$ROOT/mismatch.err"
MISMATCH_RC=$?
set -e
[ "$MISMATCH_RC" -eq 13 ] \
    || fail "guard mismatch returned rc $MISMATCH_RC instead of 13"
grep -Fq '"guard_mismatch":true' "$ROOT/mismatch.err" \
    || fail "guard mismatch omitted structured JSON: $(cat "$ROOT/mismatch.err")"
UNCHANGED=$(bd show contract-guard --json)
printf '%s\n' "$UNCHANGED" | grep -Fq '"title": "Guarded field update"' \
    || fail "stale guarded update changed state: $UNCHANGED"

bd update contract-guard --claim \
    --if-status open --if-assignee '' --json >/dev/null
CLAIMED=$(bd show contract-guard --json)
printf '%s\n' "$CLAIMED" | grep -Fq '"status": "in_progress"' \
    || fail "guarded claim did not enter in_progress: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Fq '"assignee": "Scribe Contract Actor"' \
    || fail "guarded claim did not use resolved local actor: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Eq '"lease_expires_at": "[^" ]+' \
    || fail "guarded claim lost native lease semantics: $CLAIMED"

bd close contract-guard --reason 'Guarded contract close' \
    --if-status in_progress --if-assignee 'Scribe Contract Actor' \
    --json >/dev/null
CLOSED=$(bd show contract-guard --json)
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "guarded close did not close issue: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Fq '"close_reason": "Guarded contract close"' \
    || fail "guarded close lost native reason: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Eq '"closed_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "guarded close omitted closed_at: $CLOSED"

set +e
bd reopen contract-guard --if-status in_progress \
    --if-assignee 'Scribe Contract Actor' --json \
    >"$ROOT/reopen-mismatch.out" 2>"$ROOT/reopen-mismatch.err"
REOPEN_MISMATCH_RC=$?
set -e
[ "$REOPEN_MISMATCH_RC" -eq 13 ] \
    || fail "reopen mismatch returned rc $REOPEN_MISMATCH_RC instead of 13"
STILL_CLOSED=$(bd show contract-guard --json)
printf '%s\n' "$STILL_CLOSED" | grep -Fq '"status": "closed"' \
    || fail "stale guarded reopen changed state: $STILL_CLOSED"

bd reopen contract-guard --if-status closed \
    --if-assignee 'Scribe Contract Actor' --json >/dev/null
REOPENED=$(bd show contract-guard --json)
printf '%s\n' "$REOPENED" | grep -Fq '"status": "open"' \
    || fail "guarded reopen did not reopen issue: $REOPENED"
if printf '%s\n' "$REOPENED" | grep -Eq '"closed_at": "|"close_reason": "Guarded contract close"'; then
    fail "guarded reopen retained close lifecycle fields: $REOPENED"
fi
printf '%s\n' "$REOPENED" | grep -Fq '"atomic-label"' \
    || fail "lifecycle transitions lost guarded label state: $REOPENED"

echo 'PASS: patched bd preserves atomic guards and native write lifecycles'
