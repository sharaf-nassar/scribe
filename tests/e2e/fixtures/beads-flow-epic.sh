#!/bin/bash
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Flow epic fixture]]
# bd refuses cycles even with --no-cycle-check, so this real-bd fixture covers
# the two other admitted-shape refusals: disconnected and external-blocker.
set -euo pipefail

BEADS_FLOW_ROOT=${BEADS_FLOW_ROOT:-/tmp/scribe-beads-flow-root}
BEADS_FLOW_PROJECT="$BEADS_FLOW_ROOT/flow-epic"
export BD_NO_DAEMON=1

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

case "$BEADS_FLOW_PROJECT" in
    /tmp/*) ;;
    *) fail "BEADS_FLOW_ROOT must stay under /tmp: $BEADS_FLOW_ROOT" ;;
esac

seed_beads_flow_epic_fixture() {
    rm -rf "$BEADS_FLOW_PROJECT"
    mkdir -p "$BEADS_FLOW_PROJECT"
    git -C "$BEADS_FLOW_PROJECT" init --quiet
    git -C "$BEADS_FLOW_PROJECT" config user.email e2e@example.invalid
    git -C "$BEADS_FLOW_PROJECT" config user.name 'Scribe E2E'

    (
        cd "$BEADS_FLOW_PROJECT"
        bd init --quiet --stealth --prefix flow
        bd create 'Flow fixture epic' --id flow-epic --type epic --priority 2 >/dev/null
        for issue_id in \
            flow-foundation \
            flow-api \
            flow-ui \
            flow-data \
            flow-integration \
            flow-verification \
            flow-release
        do
            bd create "$issue_id fixture" --id "$issue_id" --type task --priority 2 >/dev/null
            bd update "$issue_id" --parent flow-epic >/dev/null
        done

        bd close flow-foundation --reason 'Closed fixture blocker.' >/dev/null
        bd dep add flow-api flow-foundation >/dev/null
        bd dep add flow-ui flow-foundation >/dev/null
        bd dep add flow-data flow-foundation >/dev/null
        bd dep add flow-integration flow-api >/dev/null
        bd dep add flow-integration flow-ui >/dev/null
        bd dep add flow-integration flow-data >/dev/null
        bd dep add flow-verification flow-integration >/dev/null
        bd dep add flow-release flow-verification >/dev/null

        bd create 'Flow inadmissible fixture epic' --id flow-inadmissible-epic \
            --type epic --priority 2 >/dev/null
        bd create 'Disconnected Flow member' --id flow-disconnected --type task --priority 2 >/dev/null
        bd create 'External Flow blocker' --id flow-external-blocker --type task --priority 2 >/dev/null
        bd create 'Externally blocked Flow member' --id flow-external-blocked \
            --type task --priority 2 >/dev/null
        bd update flow-disconnected --parent flow-inadmissible-epic >/dev/null
        bd update flow-external-blocked --parent flow-inadmissible-epic >/dev/null
        bd dep add flow-external-blocked flow-external-blocker >/dev/null
    )
}

verify_beads_flow_epic_fixture() {
    # The only claim worth re-checking is one about bd itself, not about this
    # file: a blocker that is already closed stays on the dependency graph but
    # drops out of `bd blocked`. That asymmetry is why Flow reads the full list
    # rather than the blocked view, so it is pinned here.
    #
    # The epic's shape - ranks, fan-out, fan-in, the closed blocker, and the two
    # inadmissible members - is written deterministically above and asserted
    # against the real server in tests/e2e/func/beads-board.sh. Re-deriving it
    # here proved only that the literals a few lines up are still the literals a
    # few lines up.
    python3 - "$BEADS_FLOW_PROJECT" <<'PY'
import json
import subprocess
import sys

project = sys.argv[1]
payload = json.loads(subprocess.check_output(
    ["bd", "--readonly", "--json", "blocked"], cwd=project, text=True,
))
data = payload.get("data", payload) if isinstance(payload, dict) else payload
issues = data["issues"] if isinstance(data, dict) else data
if "flow-api" in {issue["id"] for issue in issues}:
    raise SystemExit("FAIL: bd blocked reported a satisfied blocker")

print("PASS: bd blocked omits the satisfied blocker the Flow graph keeps")
PY
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    seed_beads_flow_epic_fixture
    verify_beads_flow_epic_fixture
fi
