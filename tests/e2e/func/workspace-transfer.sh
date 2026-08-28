#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# e2e-timeout: 180
set -euo pipefail

# @lat: [[test#Workspace Transfer Server Transaction]]
# Replace the entrypoint's generic daemon/server pair with one disposable server
# whose agent-world policy is explicit. The harness then drives real framed
# clients through reconnect, a real --upgrade fd handoff, and target-window
# claim; no production socket or network is reachable in this container.
scribe-test daemon stop >/dev/null 2>&1 || true
scribe-test server stop >/dev/null 2>&1 || true
mkdir -p "$HOME/.config/scribe"
cat >"$HOME/.config/scribe/config.toml" <<'TOML'
[agent_api]
read_metadata = "allow"
TOML
scribe-test server start

EVIDENCE=/output/workspace-transfer.json
scribe-test workspace-transfer --evidence "$EVIDENCE"
/usr/bin/python3 - "$EVIDENCE" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    evidence = json.load(fh)
assert evidence["schema_version"] == 2
assert evidence["status"] == "pass"
expected = {
    "reconnect_tree_persisted",
    "upgrade_with_transfer_in_flight",
    "lost_ack_retry_replayed_after_upgrade",
    "source_and_target_trees_atomic",
    "old_client_capability_refused_without_mutation",
    "old_server_capability_defaults_false",
    "new_messages_decode_in_old_schemas",
    "agent_world_window_ids_flipped_atomically",
    "existing_target_edge_insert_preserves_identity_tree_and_env",
    "bidirectional_centre_swap_preserves_both_trees",
    "sole_source_reattach_acknowledges_source_close",
    "workspace_move_never_creates_a_replacement_session",
    "legacy_workspace_move_refusal_leaves_state_unchanged",
    "agent_world_and_siblings_flip_workspace_owner",
    "stale_source_input_cannot_reach_moved_pty",
}
assert expected <= set(evidence["checks"]), evidence
assert evidence["source_window_id"] != evidence["target_window_id"]
print(json.dumps(evidence, sort_keys=True))
PY

echo "PASS: workspace transfer reconnect, upgrade, compatibility, and agent-world oracles"
