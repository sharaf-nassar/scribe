#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Terminal Image Safety and Continuity#Docker Evidence Entry Point]]
set -euo pipefail

# The live functional corpus for spec 020. Every other terminal-image func
# script drives production code in-process; this one is the only place where a
# real scribe-server, a real PTY, a real detach, a real hot-reload, and a real
# SSH transport are in the loop at once. It therefore asserts continuity — what
# a session keeps across those events — rather than re-deriving unit behavior.

FIXTURES=/tests/fixtures/terminal-images
OUT=/output/terminal-images
EVIDENCE="$OUT/functional.json"
SERVER_LOG="$OUT/functional-server.log"
WORK=/tmp/terminal-images-functional
mkdir -p "$OUT" "$WORK"
: >"$SERVER_LOG"

fail() {
    echo "FAIL: $*" >&2
    tail -40 "$SERVER_LOG" >&2 2>/dev/null || true
    exit 1
}

# Case results in declaration order; the evidence manifest is written from this.
CASES=()
pass_case() {
    CASES+=("$1")
    echo "PASS: $1"
}

# ---------------------------------------------------------------------------
# Fixture and session plumbing.
# ---------------------------------------------------------------------------

# The owned fixtures are ASCII hex. `%b` expands the escapes in the argument
# without rescanning the result for printf conversions, so a 0x25 byte in a
# payload cannot turn into a format directive.
#
# A third argument rewrites the Kitty image id `i=1` to that digit before
# decoding. Re-transmitting one id replaces the image it names, so the same
# pinned fixture has to be re-identified when a phase needs the grid to grow
# instead of the definition to be replaced.
hex_to_bin() {
    local hex
    hex=$(tr -d ' \n\r' <"$1")
    [ -z "${3:-}" ] || hex=${hex//693d31/693d3$3}
    printf '%b' "$(printf '%s' "$hex" | sed 's/../\\x&/g')" >"$2"
}

# The server's tracing writer colorizes even into a file, which puts SGR
# sequences between a field name and its value. Read every log assertion off
# the de-colorized text instead of matching around them.
plain_log() { tail -n "+${1:-1}" "$SERVER_LOG" | sed 's/\x1b\[[0-9;]*m//g'; }

# Highest value a named evidence field reached in the server log at or after
# line $2. The leading space keeps `classic_placements` from also matching
# `placeholder_placements`.
log_field_max() {
    local field="$1" value
    value=$(plain_log "${2:-1}" | sed -n "s/.* $field=\([0-9][0-9]*\).*/\1/p" | sort -n | tail -1)
    printf '%s' "${value:-0}"
}

# Value of a named field on one already-extracted log line. Handoff seam counts
# are point-in-time rather than monotonic, so they have to be read off the line
# that states them instead of through `log_field_max`.
line_field() { printf '%s' "$2" | sed -n "s/.* $1=\([0-9][0-9]*\).*/\1/p"; }

log_lines() { wc -l <"$SERVER_LOG" | tr -d ' '; }

wait_log() {
    local pattern="$1" from="$2" deadline=$((SECONDS + ${3:-10}))
    until plain_log "$from" | grep -qF "$pattern"; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

wait_field_at_least() {
    local field="$1" want="$2" from="$3" deadline=$((SECONDS + ${4:-10}))
    until [ "$(log_field_max "$field" "$from")" -ge "$want" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

# Type one command into the session, discarding whatever is already on the
# input line first. Graphics replies are written back to the PTY as input, so
# an unread success or error report sits in front of the next command and would
# otherwise turn it into a syntax error the harness reads as a hang.
send_line() { scribe-test send "$SESSION" "\x15$1\n"; }

# Run one helper script inside the session's own shell and wait for its
# sentinel, so every emission below is real application output on a real PTY.
run_in_session() {
    local script="$1" sentinel="$2" timeout="${3:-8000}"
    send_line "bash $script"
    scribe-test wait-output "$SESSION" "$sentinel" --timeout "$timeout" ||
        fail "session step $script never reached $sentinel"
}

# The tty echoes the command as well as its output, so a marker that is spelled
# out in the command line proves only that keys arrived. Assemble it in the
# shell instead: the echoed text holds the format, the output holds the marker.
assert_session_alive() {
    send_line "echo ALIV''E-$1"
    scribe-test wait-output "$SESSION" "ALIVE-$1" --timeout 5000 ||
        fail "the session did not run a command after $1"
}

for fixture in kitty-query-order kitty-rgb-classic malformed-recovery; do
    hex_to_bin "$FIXTURES/$fixture.hex" "$WORK/$fixture.bin"
done
for id in 2 3 4; do
    hex_to_bin "$FIXTURES/kitty-rgb-classic.hex" "$WORK/rgb-$id.bin" "$id"
done

# A Kitty transmit claiming 4097 pixels of width — one past the frozen
# max_width_pixels — with a payload far too small to be that image. The
# rejection has to happen on the declaration, before anything is retained.
printf '\x1b_Ga=T,f=24,s=4097,v=1,t=d;AAAA\x1b\\' >"$WORK/overflow.bin"

# Emit a payload and stop; used wherever only the state effect matters.
cat >"$WORK/emit.sh" <<'EMIT'
cat "$1"
printf '\nEMIT-DONE-%s\n' "$2"
EMIT

# Emit a payload, then read what the terminal writes back on the PTY. The tty
# must leave canonical mode first: a discovery reply carries no newline, so a
# cooked line discipline would never hand it to the reader — and would not over
# an SSH pty either.
cat >"$WORK/probe.sh" <<'PROBE'
saved=$(stty -g)
stty raw -echo
cat "$2"
reply=''
IFS= read -r -t 3 -d c reply || true
stty "$saved"
printf '%sc' "$reply" | cat -v >"$1"
printf '\nPROBE-DONE\n'
PROBE

# ---------------------------------------------------------------------------
# Phase 0: a live server whose log is readable, and a capable viewer.
#
# Only a capable viewer latches a session, and only a latched session parses
# graphics at all. Terminal images are on by default, so the corpus only has to
# replace the runtime to pick up a readable server log; the restarted daemon
# announces the renderer from `terminal.images.enabled`.
# ---------------------------------------------------------------------------
scribe-test daemon stop
scribe-test server stop
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
scribe-test server start
scribe-test daemon start
SESSION=$(scribe-test session create)
assert_session_alive session-start
[ -s "$SERVER_LOG" ] || fail "the live server wrote no log"
pass_case live_capable_viewer

# ---------------------------------------------------------------------------
# Phase 1: protocol safety. A CAN-aborted APC and a SUB-aborted DCS must fail
# in a typed, bounded way without eating the application text around them.
# ---------------------------------------------------------------------------
MARK=$(($(log_lines) + 1))
FAILURES_BEFORE=$(log_field_max failures)
run_in_session "$WORK/emit.sh $WORK/malformed-recovery.bin malformed" "EMIT-DONE-malformed"
# Two aborted strings, two typed failures, and nothing retained from either.
wait_field_at_least failures $((FAILURES_BEFORE + 2)) "$MARK" ||
    fail "the aborted control strings were not recorded as typed failures"
for survivor in BEFORE AFTER TAIL; do
    scribe-test wait-output "$SESSION" "$survivor" --timeout 3000 ||
        fail "adjacent text $survivor did not survive the aborted control string"
done
assert_session_alive malformed-graphics
plain_log | grep -q 'panic' && fail "the server panicked on malformed graphics"
pass_case protocol_safety_recovery

# ---------------------------------------------------------------------------
# Phase 2: overflow. An over-limit declaration is refused before retention, and
# the very next well-formed image still lands.
# ---------------------------------------------------------------------------
MARK=$(($(log_lines) + 1))
FAILURES_BEFORE=$(log_field_max failures)
run_in_session "$WORK/emit.sh $WORK/overflow.bin overflow" "EMIT-DONE-overflow"
wait_field_at_least failures $((FAILURES_BEFORE + 1)) "$MARK" ||
    fail "an over-limit image declaration was not refused"
[ "$(log_field_max classic_placements "$MARK")" -eq 0 ] ||
    fail "an over-limit declaration retained a placement"
run_in_session "$WORK/emit.sh $WORK/kitty-rgb-classic.bin recover" "EMIT-DONE-recover"
wait_field_at_least classic_placements 1 "$MARK" ||
    fail "a well-formed image did not land after an overflow rejection"
pass_case bounded_overflow

# ---------------------------------------------------------------------------
# Phase 3: replies. The Kitty result precedes the augmented DA1 on the same
# PTY, in the order an application that probes and asks immediately sees them.
# ---------------------------------------------------------------------------
MARK=$(($(log_lines) + 1))
run_in_session "$WORK/probe.sh $OUT/functional-probe-local.txt $WORK/kitty-query-order.bin" \
    "PROBE-DONE"
LOCAL_REPLY=$(cat "$OUT/functional-probe-local.txt")
case "$LOCAL_REPLY" in
    *'_Gi=31;OK'*'[?6;4c') : ;;
    *) fail "local probe did not read OK before an attribute-4 DA1: $LOCAL_REPLY" ;;
esac
wait_field_at_least replies 1 "$MARK" || fail "the server recorded no PTY reply"
pass_case reply_order_and_discovery

# ---------------------------------------------------------------------------
# Phase 4: viewerless retention. The only viewer leaves while an application is
# still emitting; the session must keep parsing, keep its latch, and be whole
# when a viewer comes back.
# ---------------------------------------------------------------------------
PLACEMENTS_BEFORE=$(log_field_max classic_placements)
TRANSFERS_BEFORE=$(log_field_max kitty_transfers)
cat >"$WORK/detached.sh" <<'DETACHED'
(sleep 2; cat "$1"; printf '\nEMIT-DONE-detached\n') >/dev/tty 2>&1 &
printf '\nDETACHED-ARMED\n'
DETACHED
run_in_session "$WORK/detached.sh $WORK/rgb-2.bin" "DETACHED-ARMED"
MARK=$(($(log_lines) + 1))
scribe-test daemon stop
sleep 4
wait_field_at_least kitty_transfers $((TRANSFERS_BEFORE + 1)) "$MARK" 5 ||
    fail "a viewerless session stopped decoding graphics"
wait_field_at_least classic_placements $((PLACEMENTS_BEFORE + 1)) "$MARK" 5 ||
    fail "a viewerless session stopped retaining image state"
scribe-test daemon start
scribe-test session attach "$SESSION"
assert_session_alive viewer-return
pass_case viewerless_retention_and_attach

# ---------------------------------------------------------------------------
# Phase 5: sharing. Zero, one, and several viewers reading the same committed
# burst off their own queues is a receipt the landed fan-out probe already
# collects against the production sink set; run it inside this pass rather than
# building a second oracle for it.
# ---------------------------------------------------------------------------
bash /tests/terminal-image-replies-sharing.sh >"$OUT/functional-sharing.log" 2>&1 || {
    tail -30 "$OUT/functional-sharing.log" >&2
    fail "the viewer fan-out corpus did not pass inside the live run"
}
pass_case viewer_fanout

# ---------------------------------------------------------------------------
# Phase 6: local-only posture. Everything up to here ran with no network
# transport at all; the SSH case below is the single deliberate loopback
# exception, so the boundary is measured before it is opened.
# ---------------------------------------------------------------------------
off_box_endpoints() {
    # /proc/net/tcp* columns 2 and 3 are the local and remote endpoints. An
    # all-zero address is unbound or unconnected; 0100007F and ::1 are
    # loopback. Anything else is a socket pointed off this container.
    awk 'FNR > 1 { print $2; print $3 }' /proc/net/tcp /proc/net/tcp6 2>/dev/null |
        grep -v -e '^00000000:' -e '^0100007F:' \
            -e '^00000000000000000000000000000000:' \
            -e '^00000000000000000000000001000000:' || true
}
OFFNET=$(off_box_endpoints)
[ -z "$OFFNET" ] || fail "the image corpus opened a non-loopback connection: $OFFNET"
pass_case network_disabled_local_only

# ---------------------------------------------------------------------------
# Phase 7: SSH. Graphics bytes and their replies must cross a real pty-over-SSH
# hop unchanged, which is the one transport v1 promises.
# ---------------------------------------------------------------------------
mkdir -p /root/.ssh /run/sshd
chmod 700 /root/.ssh
[ -f /etc/ssh/ssh_host_ed25519_key ] ||
    ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
printf 'PermitRootLogin yes\nPasswordAuthentication no\n' >/etc/ssh/sshd_config.d/e2e.conf
/usr/sbin/sshd
cat >"$WORK/ssh.sh" <<'SSHRUN'
ssh -tt -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    root@127.0.0.1 "bash $1 $2 $3" </dev/tty
printf '\nSSH-DONE\n'
SSHRUN
MARK=$(($(log_lines) + 1))
run_in_session "$WORK/ssh.sh $WORK/probe.sh $OUT/functional-probe-ssh.txt $WORK/kitty-query-order.bin" \
    "SSH-DONE" 20000
SSH_REPLY=$(cat "$OUT/functional-probe-ssh.txt")
case "$SSH_REPLY" in
    *'_Gi=31;OK'*'[?6;4c') : ;;
    *) fail "the discovery reply did not survive the SSH pty: $SSH_REPLY" ;;
esac
# A placement lives on the grid, and an SSH login banner scrolls the grid, so
# the decode counter — not the placement count — is what says the transmission
# crossed the hop intact.
TRANSFERS_BEFORE=$(log_field_max kitty_transfers)
run_in_session "$WORK/ssh.sh $WORK/emit.sh $WORK/rgb-3.bin ssh" "SSH-DONE" 20000
wait_field_at_least kitty_transfers $((TRANSFERS_BEFORE + 1)) "$MARK" ||
    fail "an image transmitted over SSH was never decoded"
[ "$(log_field_max classic_placements "$MARK")" -ge 1 ] ||
    fail "an image transmitted over SSH produced no placement"
pass_case ssh_transport

# ---------------------------------------------------------------------------
# Phase 8: upgrade. A hot-reload must leave the session image-capable and its
# scene coherent. Coherent means all or nothing: the successor installs the
# whole committed scene, and a half-carried scene — placements naming
# definitions that did not travel — is the failure this asserts against.
#
# The handoff seam is the only place that property is observable. The export
# runs with the session's reads paused and the restore runs before the
# successor's reader consumes a byte, so nothing can move the grid between
# them. Every later reading is taken after the attach has redrawn and this
# phase's own output has scrolled the 24-row grid, and a placement that scrolls
# off the top is retired by design — so a post-attach count is a bound on the
# restored scene, never an equality with it.
# ---------------------------------------------------------------------------
MARK=$(($(log_lines) + 1))
run_in_session "$WORK/emit.sh $WORK/rgb-4.bin preupgrade" "EMIT-DONE-preupgrade"
wait_field_at_least classic_placements 1 "$MARK" ||
    fail "nothing was on the grid to carry across the upgrade"
SEAM=$(($(log_lines) + 1))
scribe-test daemon stop
scribe-test server upgrade
scribe-test daemon start
scribe-test session attach "$SESSION"
wait_log "exported terminal image state for handoff" "$SEAM" 15 ||
    fail "the upgrade exported no terminal image state"
EXPORTED=$(plain_log "$SEAM" | grep -F "exported terminal image state for handoff" | head -1)
# The export counters are registry-wide; one live session is what makes them
# comparable with the per-session restore line below.
EXPORTED_SESSIONS=$(line_field sessions "$EXPORTED")
[ "$EXPORTED_SESSIONS" = "1" ] ||
    fail "the seam exported $EXPORTED_SESSIONS sessions; the scene comparison is void"
EXPORTED_DEFINITIONS=$(line_field definitions "$EXPORTED")
EXPORTED_PLACEMENTS=$(line_field placements "$EXPORTED")
[ "$EXPORTED_PLACEMENTS" -ge 1 ] ||
    fail "nothing was on the grid to carry across the upgrade"
[ "$(line_field dropped_scenes "$EXPORTED")" = "0" ] ||
    fail "the export refused a scene it was asked to carry: $EXPORTED"
wait_log "restored terminal image state from handoff" "$SEAM" 15 ||
    fail "the successor installed none of the exported image state"
RESTORED=$(plain_log "$SEAM" | grep -F "restored terminal image state from handoff" | head -1)
RESTORED_DEFINITIONS=$(line_field definitions "$RESTORED")
RESTORED_PLACEMENTS=$(line_field placements "$RESTORED")
[ "$RESTORED_DEFINITIONS" = "$EXPORTED_DEFINITIONS" ] &&
    [ "$RESTORED_PLACEMENTS" = "$EXPORTED_PLACEMENTS" ] ||
    fail "the upgrade left a partial scene: exported" \
        "$EXPORTED_DEFINITIONS/$EXPORTED_PLACEMENTS definitions/placements," \
        "restored $RESTORED_DEFINITIONS/$RESTORED_PLACEMENTS"
SCENE_CARRIED=true
MARK=$(($(log_lines) + 1))
# The successor's counters start at zero. Make it commit a read that transmits
# nothing — a discovery query — and read back what it believes is on the grid.
# Scroll may already have retired some of the restored placements by then, so
# what this can assert is the ceiling: a server that decoded nothing must never
# hold more placements than it was handed.
run_in_session "$WORK/probe.sh $OUT/functional-probe-upgraded.txt $WORK/kitty-query-order.bin" \
    "PROBE-DONE" 15000
wait_log "terminal image application evidence" "$MARK" ||
    fail "the upgraded server committed no image read"
UPGRADED=$(plain_log "$MARK" | grep -F "terminal image application evidence" | head -1)
POST_PLACEMENTS=$(line_field classic_placements "$UPGRADED")
POST_TRANSFERS=$(line_field kitty_transfers "$UPGRADED")
[ "$POST_TRANSFERS" = "0" ] ||
    fail "the upgraded server decoded $POST_TRANSFERS transfers of its own; the check is void"
[ "$POST_PLACEMENTS" -le "$RESTORED_PLACEMENTS" ] ||
    fail "the upgraded server holds $POST_PLACEMENTS placements having decoded" \
        "nothing; only $RESTORED_PLACEMENTS were restored"
# The discovery reply proves the capability came back with the session, and a
# fresh transmission proves the successor's own pipeline is whole.
UPGRADED_REPLY=$(cat "$OUT/functional-probe-upgraded.txt")
case "$UPGRADED_REPLY" in
    *'_Gi=31;OK'*'[?6;4c') : ;;
    *) fail "an upgraded server did not answer discovery: $UPGRADED_REPLY" ;;
esac
run_in_session "$WORK/emit.sh $WORK/rgb-2.bin postupgrade" "EMIT-DONE-postupgrade"
wait_field_at_least kitty_transfers 1 "$MARK" ||
    fail "the upgraded server decoded no new transmission"
assert_session_alive server-upgrade
pass_case upgrade_continuity

# ---------------------------------------------------------------------------
# Phase 9: rollback. The master switch has to stop advertising immediately for
# a session that is already latched, leave that session's text intact, and let
# a capable viewer latch again when it is turned back on.
# ---------------------------------------------------------------------------
CONFIG="$HOME/.config/scribe/config.toml"
# Only a running client watches config.toml, and this container has none, so the
# switch is delivered the way an operator rolling back would deliver it anyway:
# write the file, hot-reload, keep the sessions. The in-process settings corpus
# owns the no-restart runtime transition.
reload_images() {
    local want="$1" mark
    printf '[terminal.images]\nenabled = %s\n' "$want" >"$CONFIG"
    mark=$(($(log_lines) + 1))
    scribe-test daemon stop
    scribe-test server upgrade
    scribe-test daemon start
    scribe-test session attach "$SESSION"
    wait_log "images_enabled=$want" "$mark" 15 ||
        fail "the server never applied terminal.images.enabled=$want"
}
reload_images false
run_in_session "$WORK/probe.sh $OUT/functional-probe-disabled.txt $WORK/kitty-query-order.bin" \
    "PROBE-DONE"
DISABLED_REPLY=$(cat "$OUT/functional-probe-disabled.txt")
case "$DISABLED_REPLY" in
    *'_G'*) fail "a disabled Scribe answered a Kitty discovery probe: $DISABLED_REPLY" ;;
    *'[?6;4c') fail "a disabled Scribe still advertised Sixel in DA1: $DISABLED_REPLY" ;;
    *'[?6c') : ;;
    *) fail "a disabled Scribe did not answer DA1 at all: $DISABLED_REPLY" ;;
esac
assert_session_alive images-disabled
# Re-enabling deliberately restores no latch; the capable viewer this reload
# reattaches is what has to earn discovery back.
reload_images true
run_in_session "$WORK/probe.sh $OUT/functional-probe-reenabled.txt $WORK/kitty-query-order.bin" \
    "PROBE-DONE"
REENABLED_REPLY=$(cat "$OUT/functional-probe-reenabled.txt")
case "$REENABLED_REPLY" in
    *'_Gi=31;OK'*'[?6;4c') : ;;
    *) fail "a re-enabled Scribe did not answer discovery again: $REENABLED_REPLY" ;;
esac
pass_case kill_switch_rollback

# ---------------------------------------------------------------------------
# The manifest. Payload-free by construction: it holds case names and counters.
# ---------------------------------------------------------------------------
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "engine": "scribe live terminal image safety and continuity",\n'
    printf '  "status": "pass",\n'
    printf '  "cases": {\n'
    for idx in "${!CASES[@]}"; do
        printf '    "%s": "pass"' "${CASES[$idx]}"
        if [ "$idx" -lt $((${#CASES[@]} - 1)) ]; then printf ','; fi
        printf '\n'
    done
    printf '  },\n'
    printf '  "observations": {\n'
    printf '    "upgrade_scene_carried": %s\n' "$SCENE_CARRIED"
    printf '  },\n'
    printf '  "counters": {\n'
    printf '    "replies": %s,\n' "$(log_field_max replies)"
    printf '    "kitty_commands": %s,\n' "$(log_field_max kitty_commands)"
    printf '    "kitty_transfers": %s,\n' "$(log_field_max kitty_transfers)"
    printf '    "failures": %s,\n' "$(log_field_max failures)"
    printf '    "classic_placements": %s\n' "$(log_field_max classic_placements)"
    printf '  }\n'
    printf '}\n'
} >"$EVIDENCE"

grep -Eq '"[A-Za-z0-9_]+": \[' "$EVIDENCE" && fail "the manifest embedded array-shaped data"
echo "PASS: terminal image safety and continuity (${#CASES[@]} cases)"
