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
    python3 - "$BEADS_FLOW_PROJECT" <<'PY'
import json
import os
import subprocess
import sys

project = sys.argv[1]
env = os.environ | {"BD_JSON_ENVELOPE": "1"}
payload = json.loads(subprocess.check_output(
    ["bd", "--readonly", "--json", "list", "--all", "--limit", "0"],
    cwd=project,
    env=env,
    text=True,
))
data = payload["data"]
issues = data["issues"] if isinstance(data, dict) else data
by_id = {issue["id"]: issue for issue in issues}

flow_members = {
    "flow-foundation",
    "flow-api",
    "flow-ui",
    "flow-data",
    "flow-integration",
    "flow-verification",
    "flow-release",
}
if any(by_id[issue_id].get("parent") != "flow-epic" for issue_id in flow_members):
    raise SystemExit("FAIL: Flow members did not retain their epic parent")

blockers = {
    issue_id: {
        dependency["depends_on_id"]
        for dependency in by_id[issue_id].get("dependencies", [])
        if dependency["type"] == "blocks"
    }
    for issue_id in flow_members
}
ranks = {"flow-foundation": 0}
while len(ranks) < len(flow_members):
    for issue_id, predecessors in blockers.items():
        if issue_id not in ranks and predecessors <= ranks.keys():
            ranks[issue_id] = 1 + max(ranks[predecessor] for predecessor in predecessors)
if max(ranks.values()) != 4:
    raise SystemExit(f"FAIL: expected five ranks, got {ranks}")
if {"flow-api", "flow-ui", "flow-data"} != {
    issue_id for issue_id, predecessors in blockers.items()
    if predecessors == {"flow-foundation"}
}:
    raise SystemExit(f"FAIL: fan-out rank missing: {blockers}")
if blockers["flow-integration"] != {"flow-api", "flow-ui", "flow-data"}:
    raise SystemExit(f"FAIL: fan-in missing: {blockers}")
if by_id["flow-foundation"]["status"] != "closed":
    raise SystemExit("FAIL: satisfied blocker was not closed")

blocked_payload = json.loads(subprocess.check_output(
    ["bd", "--readonly", "--json", "blocked"], cwd=project, text=True,
))
blocked_data = (
    blocked_payload.get("data", blocked_payload)
    if isinstance(blocked_payload, dict)
    else blocked_payload
)
blocked_issues = (
    blocked_data["issues"] if isinstance(blocked_data, dict) else blocked_data
)
if "flow-api" in {issue["id"] for issue in blocked_issues}:
    raise SystemExit("FAIL: bd blocked reported a satisfied blocker")

all_blockers = {
    issue_id: {
        dependency["depends_on_id"]
        for dependency in issue.get("dependencies", [])
        if dependency["type"] == "blocks"
    }
    for issue_id, issue in by_id.items()
}
if all_blockers["flow-disconnected"] or any(
    "flow-disconnected" in predecessors for predecessors in all_blockers.values()
):
    raise SystemExit("FAIL: disconnected member unexpectedly has a blocks edge")
if by_id["flow-external-blocked"].get("parent") != "flow-inadmissible-epic":
    raise SystemExit("FAIL: external-blocker member lost its epic parent")
if {
    dependency["depends_on_id"]
    for dependency in by_id["flow-external-blocked"].get("dependencies", [])
    if dependency["type"] == "blocks"
} != {"flow-external-blocker"}:
    raise SystemExit("FAIL: external blocker dependency missing")
if by_id["flow-external-blocker"].get("parent"):
    raise SystemExit("FAIL: external blocker joined the inadmissible epic")

print(
    "PASS: Flow fixture seeded five ranks, fan-out=3, fan-in=3, "
    "a satisfied closed blocker, and disconnected/external-blocker refusals"
)
PY
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    seed_beads_flow_epic_fixture
    verify_beads_flow_epic_fixture
fi
