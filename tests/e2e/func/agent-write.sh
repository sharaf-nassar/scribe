#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

CONFIG_FILE="$HOME/.config/scribe/config.toml"
PAYLOAD_BYTES=65536
READY=/output/agent-write-ready
GATE=/output/agent-write-gate
LANDED=/output/agent-write-landed
ACK=/output/agent-write-ack.json
STATUS=/output/agent-write-status
rm -f "$READY" "$GATE" "$LANDED" "$ACK" "$STATUS"

scribe-test daemon stop >/dev/null 2>&1 || true
scribe-test server stop >/dev/null 2>&1 || true
cat >"$CONFIG_FILE" <<TOML
[agent_api]
write_input = "allow"
max_input_bytes = $PAYLOAD_BYTES
TOML
scribe-test server start
scribe-test daemon start

CALLER=$(scribe-test session create)
TARGET=$(scribe-test session create)

# Put the target PTY in raw mode, then hold its reader behind a file gate. A
# complete 64 KiB write cannot fit in the PTY buffer while this reader is held,
# so an acknowledgement before the gate opens proves a premature reply.
TARGET_PROGRAM="import os,pathlib,time; pathlib.Path('$READY').write_text('ready'); gate=pathlib.Path('$GATE'); target=pathlib.Path('$LANDED'); n=$PAYLOAD_BYTES; [time.sleep(0.01) for _ in iter(gate.exists, True)]; read_exact=lambda left: b'' if left == 0 else (chunk:=os.read(0,left))+read_exact(left-len(chunk)); target.write_bytes(read_exact(n))"
scribe-test send "$TARGET" "stty raw -echo; python3 -c \"$TARGET_PROGRAM\"; stty sane; printf 'agent-write-target-done\\n'\n"
for _ in {1..100}; do
    [ -f "$READY" ] && break
    sleep 0.05
done
[ -f "$READY" ] || fail "target reader never reached its gate"

# The payload is generated inside the caller pane, so the CLI invocation itself
# is genuinely in-pane without injecting 64 KiB through the harness command.
scribe-test send "$CALLER" "RUST_LOG=off scribe agent --agent agent-write-e2e write '$TARGET' --text \"\$(python3 -c 'print(\"x\"*$PAYLOAD_BYTES,end=\"\")')\" > '$ACK' 2> /output/agent-write.stderr; printf '%s\\n' \"\$?\" > '$STATUS' &\n"
sleep 0.75
[ ! -s "$ACK" ] && [ ! -e "$STATUS" ] \
    || fail "write acknowledged before the blocked PTY could accept all bytes"
echo "PHASE 1 PASS: acknowledgement stayed pending while the target reader was blocked"

touch "$GATE"
for _ in {1..200}; do
    [ -s "$ACK" ] && [ -s "$STATUS" ] && [ -f "$LANDED" ] && break
    sleep 0.05
done
[ -s "$ACK" ] || fail "write never acknowledged after the target reader opened"
[ "$(cat "$STATUS")" = "0" ] || fail "write CLI failed: $(cat "$ACK")"
[ -f "$LANDED" ] || fail "target never received the write"
[ "$(wc -c <"$LANDED")" -eq "$PAYLOAD_BYTES" ] \
    || fail "target received $(wc -c <"$LANDED") bytes, expected $PAYLOAD_BYTES"
[ "$(tr -d x <"$LANDED" | wc -c)" -eq 0 ] || fail "target received bytes outside the payload"
grep -q '"ok":true' "$ACK" || fail "write acknowledgement was not successful"
grep -q '"type":"write_input"' "$ACK" || fail "write acknowledgement had the wrong payload"
scribe-test wait-output "$TARGET" "agent-write-target-done"
echo "PHASE 2 PASS: the complete payload landed before the successful acknowledgement"

echo "PASS: agent write CLI functional coverage completed"
