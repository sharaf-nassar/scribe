#!/bin/bash
# e2e-timeout: 90
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
set -euo pipefail

CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
ROOT=/work/scribe
HEAD=4f2a91c4f2a91c4f2a91c4f2a91c4f2a91c4f2a

fail() {
    echo "FAIL: $*" >&2
    tail -80 "$CLIENT_LOG" 2>/dev/null || true
    exit 1
}

window_id() {
    xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1
}

first_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys
found = None
with open(sys.argv[1]) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if row.get("dir") == "client" and msg.get("type") == "CreateSession":
            found = msg["workspace_id"]
if not found:
    raise SystemExit(1)
print(found)
PY
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.6
}

interest_count() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
want = sys.argv[2] == "true"
count = 0
with open(sys.argv[1]) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if (row.get("dir") == "client"
                and msg.get("type") == "SetCiRunDetailsInterest"
                and msg.get("repo_root") == "/work/scribe"
                and msg.get("head_sha") == "4f2a91c4f2a91c4f2a91c4f2a91c4f2a91c4f2a"
                and msg.get("interested") is want):
            count += 1
print(count)
PY
}

terminal_space_count() {
    python3 - "$RECORD" <<'PY'
import json, sys
count = 0
with open(sys.argv[1]) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if (row.get("dir") == "client"
                and msg.get("type") == "KeyInput"
                and msg.get("data") == [32]):
            count += 1
print(count)
PY
}

sleep 2
WID=$(window_id)
[ -n "$WID" ] || fail "no Scribe window"
xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID"
WORKSPACE=$(first_workspace) || fail "no visible workspace recorded"
NOW=$(date +%s)
START=$(( NOW - 102 ))

inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":\"$ROOT\"}"
inject "{\"type\":\"CiRunState\",\"repo_root\":\"$ROOT\",\"delta\":{\"Set\":{\"repository\":\"acme/scribe\",\"head_sha\":\"$HEAD\",\"branch\":\"main\",\"workflows\":[{\"run_id\":101,\"name\":\"quality\",\"status\":\"in_progress\",\"conclusion\":null,\"started_at_epoch_secs\":$START,\"updated_at_epoch_secs\":$NOW},{\"run_id\":102,\"name\":\"docs\",\"status\":\"queued\",\"conclusion\":null,\"started_at_epoch_secs\":null,\"updated_at_epoch_secs\":$NOW}],\"rollup\":\"running\",\"stale\":false}}}"
import -window "$WID" +repage /output/ci-run-details-collapsed.png

# Pointer activation uses the collapsed band's central job area. Window-relative
# motion avoids Openbox's frame/client origin offset while still driving XTEST.
xdotool mousemove --sync --window "$WID" 360 54
xdotool click 1
for _ in {1..30}; do
    [ "$(interest_count true)" -gt 0 ] && break
    sleep 0.1
done
[ "$(interest_count true)" -gt 0 ] || fail "pointer toggle sent no open interest"

inject "{\"type\":\"CiRunDetails\",\"repo_root\":\"$ROOT\",\"details\":{\"head_sha\":\"$HEAD\",\"jobs\":[{\"job_id\":201,\"workflow_run_id\":101,\"workflow_name\":\"quality\",\"name\":\"rust-linux\",\"status\":\"completed\",\"conclusion\":\"success\",\"started_at_epoch_secs\":$START,\"completed_at_epoch_secs\":$(( START + 60 )),\"steps\":[{\"name\":\"just test\",\"status\":\"completed\",\"conclusion\":\"success\"}]},{\"job_id\":202,\"workflow_run_id\":101,\"workflow_name\":\"quality\",\"name\":\"rust-macos\",\"status\":\"in_progress\",\"conclusion\":null,\"started_at_epoch_secs\":$(( START + 15 )),\"completed_at_epoch_secs\":null,\"steps\":[{\"name\":\"cargo clippy --workspace\",\"status\":\"in_progress\",\"conclusion\":null}]},{\"job_id\":203,\"workflow_run_id\":102,\"workflow_name\":\"docs\",\"name\":\"lat-check\",\"status\":\"queued\",\"conclusion\":null,\"started_at_epoch_secs\":null,\"completed_at_epoch_secs\":null,\"steps\":[]}]}}"
import -window "$WID" +repage /output/ci-run-details-expanded.png
identify /output/ci-run-details-expanded.png >/dev/null \
    || fail "expanded screenshot is unreadable"
WIDTH=$(identify -format %w /output/ci-run-details-expanded.png)
PANEL_DIFF=$(compare -metric AE -crop "${WIDTH}x116+0+74" \
    /output/ci-run-details-collapsed.png /output/ci-run-details-expanded.png null: 2>&1 || true)
PANEL_DIFF=${PANEL_DIFF%%.*}
[ "${PANEL_DIFF:-0}" -gt 8000 ] \
    || fail "expanded trace panel changed only ${PANEL_DIFF:-0} pixels"

# Pointer activation leaves focus on the toggle. Space must close it and emit
# the inverse interest without forwarding a byte to the terminal.
SPACES_BEFORE=$(terminal_space_count)
xdotool key space
for _ in {1..30}; do
    [ "$(interest_count false)" -gt 0 ] && break
    sleep 0.1
done
[ "$(interest_count false)" -gt 0 ] || fail "keyboard toggle sent no close interest"
[ "$(terminal_space_count)" -eq "$SPACES_BEFORE" ] \
    || fail "keyboard toggle forwarded Space to the terminal"

# @lat: [[test#GPUI CI Run Bar#Expanded trace visual and keyboard toggle]]
echo "PASS: pointer opened the trace, keyboard closed it, expanded screenshot captured"
