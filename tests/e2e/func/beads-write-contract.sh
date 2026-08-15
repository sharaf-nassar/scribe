#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Official Beads Write Contract]]
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
printf '%s\n' "$VERSION" | grep -Fq 'bd version 1.1.0' \
    || fail "unexpected official bd version: $VERSION"

cd "$PROJECT"
bd init --quiet --stealth --prefix contract
bd create 'Field and lifecycle issue' --id contract-field --type task --priority 2 >/dev/null
bd create 'Claim issue' --id contract-claim --type task --priority 3 >/dev/null

bd update contract-field --title 'Official field update' --add-label official --json >/dev/null
bd comments add contract-field 'official comment' --json >/dev/null
FIELD=$(bd show contract-field --json --include-comments)
printf '%s\n' "$FIELD" | grep -Fq '"title": "Official field update"' \
    || fail "official field update did not commit: $FIELD"
printf '%s\n' "$FIELD" | grep -Fq '"official"' \
    || fail "official label update did not commit: $FIELD"
printf '%s\n' "$FIELD" | grep -Fq '"text": "official comment"' \
    || fail "official comment did not commit: $FIELD"

bd update contract-claim --claim --json >/dev/null
CLAIMED=$(bd show contract-claim --json)
printf '%s\n' "$CLAIMED" | grep -Fq '"status": "in_progress"' \
    || fail "official claim did not enter in_progress: $CLAIMED"
printf '%s\n' "$CLAIMED" | grep -Fq '"assignee": "Scribe Contract Actor"' \
    || fail "official claim did not resolve the local actor: $CLAIMED"

bd close contract-claim --reason 'Official contract close' --json >/dev/null
CLOSED=$(bd show contract-claim --json)
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "official close did not close the issue: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Fq '"close_reason": "Official contract close"' \
    || fail "official close lost its reason: $CLOSED"

bd reopen contract-claim --json >/dev/null
REOPENED=$(bd show contract-claim --json)
printf '%s\n' "$REOPENED" | grep -Fq '"status": "open"' \
    || fail "official reopen did not reopen the issue: $REOPENED"
if printf '%s\n' "$REOPENED" | grep -Eq '"closed_at": "|"close_reason": "Official contract close"'; then
    fail "official reopen retained close lifecycle fields: $REOPENED"
fi

echo 'PASS: official bd preserves field, comment, claim, close, and reopen behavior'
