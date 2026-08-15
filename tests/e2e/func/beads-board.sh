#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

ROOT=/tmp/scribe-beads-root
PROJECT="$ROOT/real-board"
mkdir -p "$PROJECT"
git -C "$PROJECT" init --quiet
git -C "$PROJECT" config user.email e2e@example.invalid
git -C "$PROJECT" config user.name 'Scribe E2E'

(
    cd "$PROJECT"
    bd init --quiet --stealth --prefix e2e
    bd create 'Card detail epic' --id e2e-epic --type epic --priority 2 >/dev/null
    bd create 'Open detail blocker' --id e2e-blocker --type task --priority 0 \
        --description 'Blocks the detail fixture until its contract is met.' >/dev/null
    bd create 'Complete card detail' --id e2e-detail --type task --priority 1 \
        --description 'Description fixture for the detail panel.' \
        --acceptance 'Acceptance fixture survives bd show.' \
        --notes 'Notes fixture keeps implementation context.' \
        --design 'Design fixture documents the direct data flow.' \
        --spec-id '024-beads-card-detail' \
        --labels 'beads,detail-fixture' \
        --assignee 'fixture-owner' \
        --estimate 90 \
        --external-ref 'fixture://beads-card-detail' \
        --due '2030-02-01' >/dev/null
    bd update e2e-detail --parent e2e-epic >/dev/null
    bd dep add e2e-detail e2e-blocker >/dev/null
    bd comments add e2e-detail 'Oldest deterministic fixture comment.' \
        --author 'Fixture Author' >/dev/null
    bd comments add e2e-detail 'Newest deterministic fixture comment.' \
        --author 'Fixture Reviewer' >/dev/null
    bd create 'Detail dependent' --id e2e-dependent --type task --priority 2 >/dev/null
    bd dep add e2e-dependent e2e-detail >/dev/null
    bd create 'Closed detail fixture' --id e2e-closed --type task --priority 3 >/dev/null
    bd close e2e-closed --reason 'Closed fixture reason.' >/dev/null
    bd create 'Deferred detail fixture' --id e2e-deferred --type task --priority 4 \
        --defer '2030-03-01' >/dev/null
    bd create 'Real board refresh' --id e2e-ready --type task --priority 1 >/dev/null
)

scribe-test daemon stop
scribe-test server stop
printf '[workspaces]\nroots = ["%s"]\n' "$ROOT" >"$HOME/.config/scribe/config.toml"
export BD_NO_DAEMON=1
scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create --cwd "$PROJECT")
scribe-test wait-cwd "$SESSION" "$PROJECT"
BOARD=$(scribe-test beads-board)

printf '%s\n' "$BOARD" | grep -q '"Ready"' \
    || fail "board refresh did not reach Ready: $BOARD"
READY_LANE=${BOARD#*'"ready":['}
READY_LANE=${READY_LANE%%'],"in_progress"'*}
printf '%s\n' "$READY_LANE" | grep -Fq '"id":"e2e-ready","title":"Real board refresh"' \
    || fail "seeded ready issue was absent from the Ready lane: $BOARD"

DETAIL=$(cd "$PROJECT" && bd show e2e-detail --json --include-comments --include-dependents)
CLOSED=$(cd "$PROJECT" && bd show e2e-closed --json --include-comments --include-dependents)
DEFERRED=$(cd "$PROJECT" && bd show e2e-deferred --json --include-comments --include-dependents)

for expected in \
    '"title": "Complete card detail"' \
    '"description": "Description fixture for the detail panel."' \
    '"acceptance_criteria": "Acceptance fixture survives bd show."' \
    '"notes": "Notes fixture keeps implementation context."' \
    '"design": "Design fixture documents the direct data flow."' \
    '"spec_id": "024-beads-card-detail"' \
    '"status": "open"' \
    '"priority": 1' \
    '"issue_type": "task"' \
    '"assignee": "fixture-owner"' \
    '"owner": "e2e@example.invalid"' \
    '"created_by": "Scribe E2E"' \
    '"due_at": "2030-02-01T00:00:00Z"' \
    '"estimated_minutes": 90' \
    '"external_ref": "fixture://beads-card-detail"' \
    '"parent": "e2e-epic"' \
    '"id": "e2e-blocker"' \
    '"id": "e2e-dependent"' \
    '"author": "Fixture Author"' \
    '"text": "Oldest deterministic fixture comment."' \
    '"author": "Fixture Reviewer"' \
    '"text": "Newest deterministic fixture comment."'
do
    printf '%s\n' "$DETAIL" | grep -Fq "$expected" \
        || fail "seeded detail omitted $expected: $DETAIL"
done
printf '%s\n' "$DETAIL" | grep -Fq '"beads"' \
    || fail "seeded detail omitted beads label: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Fq '"detail-fixture"' \
    || fail "seeded detail omitted detail-fixture label: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Eq '"created_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "seeded detail omitted created_at: $DETAIL"
printf '%s\n' "$DETAIL" | grep -Eq '"updated_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "seeded detail omitted updated_at: $DETAIL"
printf '%s\n' "$CLOSED" | grep -Fq '"status": "closed"' \
    || fail "closed fixture stayed open: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Fq '"close_reason": "Closed fixture reason."' \
    || fail "closed fixture omitted its reason: $CLOSED"
printf '%s\n' "$CLOSED" | grep -Eq '"closed_at": "[0-9]{4}-[0-9]{2}-[0-9]{2}T' \
    || fail "closed fixture omitted closed_at: $CLOSED"
printf '%s\n' "$DEFERRED" | grep -Fq '"defer_until": "2030-03-01T00:00:00Z"' \
    || fail "deferred fixture omitted defer_until: $DEFERRED"

echo 'PASS: real bd refreshed the board and returned complete deterministic detail fixtures'
