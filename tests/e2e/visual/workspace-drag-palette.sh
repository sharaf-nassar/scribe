#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# e2e-timeout: 300
set -euo pipefail

# @lat: [[test#GPUI Workspace Drag]]
# All workspace moves are confirmed from the real command palette. No pointer
# input is used anywhere in this script.
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

fail() {
    echo "FAIL: $1" >&2
    tail -80 "$CLIENT_LOG" >&2 || true
    exit 1
}

find_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

focus() {
    WID=$(find_windows | head -1)
    [ -n "$WID" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.6
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

palette() {
    focus
    send_keys ctrl+shift+p
    xdotool type --clearmodifiers --delay 8 "$1"
    sleep 0.3
    send_keys Return
}

reset_record() {
    : >"$RECORD"
    sleep 0.3
}

cat >/tmp/workspace-palette-oracle.py <<'PY'
import json,sys,time
path,command=sys.argv[1:3]

def rows():
    try:
        with open(path) as fh:
            for line in fh:
                try: yield json.loads(line)
                except ValueError: pass
    except OSError: return

def leaves(node):
    if not isinstance(node,dict): return []
    if "Leaf" in node: return [node["Leaf"]]
    inner=node.get("Split",node)
    return leaves(inner.get("first"))+leaves(inner.get("second"))

def trees():
    for row in rows():
        msg=row.get("message",{})
        if row.get("dir")=="client" and msg.get("type")=="ReportWorkspaceTree": yield msg.get("tree")
        if row.get("dir")=="server" and msg.get("type")=="SessionList" and msg.get("workspace_tree"): yield msg.get("workspace_tree")

def latest():
    found=list(trees()); return found[-1] if found else None

def count(name): return sum(row.get("message",{}).get("type")==name for row in rows())

if command=="wait-leaves":
    wanted=int(sys.argv[3]); deadline=time.time()+30
    while time.time()<deadline:
        found=leaves(latest())
        if len(found)==wanted and all(x.get("session_ids") for x in found):
            print(" ".join(str(x["workspace_id"]) for x in found)); sys.exit(0)
        time.sleep(.2)
    sys.exit(1)
if command=="wait-report":
    deadline=time.time()+25
    while time.time()<deadline:
        if count("ReportWorkspaceTree"):
            print(json.dumps(latest(),sort_keys=True,separators=(",",":"))); sys.exit(0)
        time.sleep(.2)
    sys.exit(1)
if command=="count": print(count(sys.argv[3]))
if command=="leaf":
    wanted=sys.argv[3]
    for leaf in leaves(latest()):
        if str(leaf.get("workspace_id"))==wanted:
            print(json.dumps(leaf,sort_keys=True,separators=(",",":"))); sys.exit(0)
    sys.exit(1)
if command=="assert-transfer":
    wanted,leaf_path=sys.argv[3:5]
    source_leaf=json.load(open(leaf_path)); sessions=set(source_leaf.get("session_ids",[]))
    result=False; target=False; creates=0; created=set(); known=set()
    for row in rows():
        msg=row.get("message",{})
        if row.get("dir")=="client" and msg.get("type")=="CreateSession": creates+=1
        if row.get("dir")=="server" and msg.get("type")=="SessionCreated": created.add(msg.get("session_id"))
        if row.get("dir")=="server" and msg.get("type")=="SessionList":
            known.update(session.get("session_id") for session in msg.get("sessions", []))
        if row.get("dir")=="server" and msg.get("type")=="WorkspaceTransferResult" and str(msg.get("result", "")).lower()=="transferred": result=True
        tree=None
        if row.get("dir")=="client" and msg.get("type")=="ReportWorkspaceTree": tree=msg.get("tree")
        if row.get("dir")=="server" and msg.get("type")=="SessionList": tree=msg.get("workspace_tree")
        found=leaves(tree) if tree else []
        if len(found)==1 and str(found[0].get("workspace_id"))==wanted:
            target=json.dumps(found[0],sort_keys=True,separators=(",",":"))==json.dumps(source_leaf,sort_keys=True,separators=(",",":"))
    assert result and target,(result,target)
    assert creates==0,creates
    assert created <= (known | sessions), created - known - sessions
    print(json.dumps({"result":"Transferred","target_leaf_exact":True,"create_session_frames":0,"session_created_ids_preexisting":True},sort_keys=True))
PY
oracle() { python3 /tmp/workspace-palette-oracle.py "$RECORD" "$@"; }

focus
oracle wait-leaves 1 >/dev/null || fail "initial workspace never became live"

# No-neighbor feedback is a real palette path too: no pointer, no tree report.
reset_record
palette "Move workspace left"
[ "$(oracle count ReportWorkspaceTree)" -eq 0 ] \
    || fail "no-neighbor palette move changed the tree"
echo "PHASE 0 PASS: no-neighbor palette move was a no-op"

# Build an L-shaped three-region fixture with keyboard chords only. The newest
# workspace stays focused across every move.
send_keys ctrl+alt+backslash
send_keys ctrl+alt+minus
ids=$(oracle wait-leaves 3) || fail "three-workspace palette fixture did not become live"
c=$(printf '%s\n' "$ids" | awk '{ print $3 }')
[ -n "$c" ] || fail "focused workspace id missing"

phase=1
for direction in up down left right; do
    reset_record
    palette "Move workspace $direction"
    oracle wait-report >"/output/workspace-palette-$direction-tree.json" \
        || fail "Move workspace $direction produced no tree report"
    [ "$(oracle count KeyInput)" -eq 0 ] \
        || fail "Move workspace $direction leaked palette keys into the PTY"
    echo "PHASE $phase PASS: Move workspace $direction executed from the palette"
    phase=$((phase + 1))
done

# The fifth row transfers the same focused workspace into a fresh window. Save
# its complete leaf before release and require an exact target tree afterwards.
source_leaf=/output/workspace-palette-source-leaf.json
oracle leaf "$c" >"$source_leaf" || fail "focused workspace leaf missing before palette tear-out"
reset_record
palette "Move workspace to new window"
for _ in $(seq 1 100); do
    mapfile -t windows < <(find_windows)
    [ "${#windows[@]}" -eq 2 ] && break
    sleep 0.3
done
[ "${#windows[@]}" -eq 2 ] || fail "palette tear-out did not map a second window"
oracle assert-transfer "$c" "$source_leaf" >/output/workspace-palette-tearout.json \
    || fail "palette tear-out did not preserve the focused workspace leaf"
[ "$(oracle count KeyInput)" -eq 0 ] \
    || fail "Move workspace to new window leaked palette keys into the PTY"
echo "PHASE 5 PASS: Move workspace to new window executed from the palette"

echo "PASS: all five workspace actions executed palette-only"
