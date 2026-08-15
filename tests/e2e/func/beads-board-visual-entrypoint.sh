#!/bin/bash
set -euo pipefail

export SCRIBE_E2E_SANDBOX=1
export BD_NO_DAEMON=1
/tests/func/beads-board.sh --seed
: >/output/share-wire.jsonl

export SCRIBE_BEADS_PRESEEDED=1
export SCRIBE_EXTRA_CONFIG=$'[workspaces]\nroots = ["/tmp/scribe-beads-root"]'
export HOME=/tmp/scribe-beads-root/real-board
exec /entrypoint.sh "$@"
