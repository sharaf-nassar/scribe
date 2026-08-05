#!/bin/bash
# e2e-timeout: 900
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[test#Pinned Terminal Image Application Corpus#Pinned applications reach a working image path]]
set -euo pipefail

OUT=/output/terminal-images/linux/apps
SERVER_LOG=/output/server.log
CLIENT_LOG=/output/client-images.log
STEPS=/tmp/image-apps
EVIDENCE="$OUT/steps.tsv"
mkdir -p "$OUT" "$STEPS"
: >"$EVIDENCE"

fail() {
    echo "FAIL: $1" >&2
    tail -60 "$CLIENT_LOG" 2>/dev/null >&2 || true
    grep -F 'terminal image' "$SERVER_LOG" 2>/dev/null | tail -40 >&2 || true
    exit 1
}

wait_file() {
    local path="$1" timeout_secs="$2" started
    started=$(date +%s)
    until [ -e "$path" ]; do
        [ $(( "$(date +%s)" - started )) -lt "$timeout_secs" ] || return 1
        kill -0 "$CLIENT_PID" 2>/dev/null || return 1
        sleep 0.15
    done
}

wait_client_log() {
    local pattern="$1" timeout_secs="$2" started
    started=$(date +%s)
    until grep -qF "$pattern" "$CLIENT_LOG" 2>/dev/null; do
        [ $(( "$(date +%s)" - started )) -lt "$timeout_secs" ] || return 1
        kill -0 "$CLIENT_PID" 2>/dev/null || return 1
        sleep 0.2
    done
}

focus() {
    local wid
    wid=$(xdotool search --name '^Scribe$' 2>/dev/null | tail -1)
    [ -n "$wid" ] || fail "no Scribe client window"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    printf '%s' "$wid"
}

# Highest value a named evidence field reached inside one step's log window.
# The leading space is what keeps `classic_placements` from also matching
# `placeholder_placements`, and `sixel_images` from matching `sixel_placements`.
field_max() {
    local field="$1" value
    value=$(sed -n "s/.* $field=\([0-9][0-9]*\).*/\1/p" "$STEPS/window.log" | sort -n | tail -1)
    printf '%s' "${value:-0}"
}

# ---------------------------------------------------------------------------
# The pinned corpus, its owned fixture, and the in-container SSH endpoint.
# ---------------------------------------------------------------------------

# The versions the frozen contract pins. A repository that silently moved would
# make every later assertion a statement about a different program.
yazi --version | grep -qF 'Yazi 26.5.6' || fail "yazi is not the pinned v26.5.6: $(yazi --version | head -1)"
chafa --version | grep -qF 'Chafa version 1.18.2' || fail "chafa is not the pinned 1.18.2"
gnuplot --version | grep -qF 'gnuplot 6.0 patchlevel 3' || fail "gnuplot is not the pinned 6.0.3"

mkdir -p "$STEPS/pictures"
convert -size 64x64 xc:'#d02020' "$STEPS/pictures/red.png"

# The owned Unicode-placeholder fixture, not an application: no released
# terminal-image application drives Kitty's virtual placements through an
# unrecognised terminal, so placeholder semantics are proven by Scribe's own
# frozen corpus byte-for-byte.
python3 - "$STEPS/placeholder.bin" <<'PY'
import pathlib
import sys

source = pathlib.Path("/tests/fixtures/terminal-images/kitty-unicode-placeholder.hex")
pathlib.Path(sys.argv[1]).write_bytes(bytes.fromhex(source.read_text().strip()))
PY

# A real sshd, reached over loopback with a real key. TERM travels through the
# ssh pty request untouched, so an application on the far side sees exactly the
# unknown terminal it would see anywhere else — no spoofing anywhere.
ssh-keygen -A >/dev/null 2>&1
mkdir -p /root/.ssh /run/sshd
chmod 700 /root/.ssh
ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
printf 'PermitRootLogin yes\nPasswordAuthentication no\n' >/etc/ssh/sshd_config.d/e2e.conf
/usr/sbin/sshd
SSH='ssh -tt -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1'

write_step() {
    local name="$1" body="$2"
    { printf 'clear\n%s\ntouch %s/done-%s\n' "$body" "$STEPS" "$name"; } >"$STEPS/$name.sh"
}

write_step placeholder "cat $STEPS/placeholder.bin"
write_step chafa-kitty "chafa --format kitty --probe off --size 12x6 $STEPS/pictures/red.png"
write_step chafa-sixel "chafa --format sixels --probe off --size 12x6 $STEPS/pictures/red.png"
write_step gnuplot-sixel "gnuplot -e 'set terminal sixelgd size 240,180; set output; plot sin(x)'"
# `--foreground` is load-bearing: without it timeout puts yazi in its own
# process group, the first controlling-terminal read raises SIGTTIN, and the
# stopped TUI never gets as far as its Kitty probe.
write_step yazi "timeout --foreground -k 1 5 yazi $STEPS/pictures || true"
write_step chafa-ssh "$SSH 'chafa --format kitty --probe off --size 12x6 $STEPS/pictures/red.png; chafa --format sixels --probe off --size 12x6 $STEPS/pictures/red.png'"
write_step gnuplot-ssh "$SSH \"gnuplot -e 'set terminal sixelgd size 240,180; set output; plot cos(x)'\""
write_step yazi-ssh "$SSH 'timeout --foreground -k 1 5 yazi $STEPS/pictures || true'"

# ---------------------------------------------------------------------------
# Replace the container-local client with an image-capable one.
#
# Capability is what latches a session: until a viewer announces the renderer
# subset, the server leaves the session text-only, answers no discovery probe,
# and every pinned application correctly falls back to text. Terminal images
# are on by default, so a plain client relaunch announces the renderer.
# ---------------------------------------------------------------------------
kill "${SCRIBE_CLIENT_PID:?visual entrypoint did not export SCRIBE_CLIENT_PID}" 2>/dev/null || true
wait "$SCRIBE_CLIENT_PID" 2>/dev/null || true
: >"$CLIENT_LOG"
LIBGL_ALWAYS_SOFTWARE=1 \
    scribe-client >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!
trap 'kill "$CLIENT_PID" 2>/dev/null || true' EXIT

wait_client_log "pane adopted a session" 30 || fail "image-capable client never adopted a session"
sleep 1.5
focus >/dev/null

# Prove the pane is live and typed input lands before any protocol assertion,
# so a broken rig cannot be mistaken for an application that emitted nothing.
write_step ready ':'
xdotool type --delay 8 --clearmodifiers "bash $STEPS/ready.sh"
xdotool key --clearmodifiers Return
wait_file "$STEPS/done-ready" 20 || fail "typed input never reached the client's pane"

run_step() {
    local name="$1" before wid
    before=$(wc -l <"$SERVER_LOG")
    wid=$(focus)
    xdotool type --delay 8 --clearmodifiers "bash $STEPS/$name.sh"
    xdotool key --clearmodifiers Return
    wait_file "$STEPS/done-$name" 30 || fail "$name never completed in the pane"
    sleep 0.8
    tail -n "+$(( before + 1 ))" "$SERVER_LOG" \
        | sed -e 's/\x1b\[[0-9;]*m//g' \
        | grep -F 'terminal image application evidence' >"$STEPS/window.log" || true
    import -window "$wid" "$OUT/$name.png"
}

# Cumulative counters carried across steps; a step proves its own increment.
replies=0
kitty_commands=0
kitty_transfers=0
sixel_images=0
failures=0

assert_step() {
    local name="$1" want_kitty="$2" want_sixel="$3" want_replies="$4" want_commands="$5"
    local want_kind="$6"
    local new_replies new_kitty new_sixel new_failures new_commands
    local classic placeholder sixel_place kind_count
    new_replies=$(field_max replies)
    new_commands=$(field_max kitty_commands)
    new_kitty=$(field_max kitty_transfers)
    new_sixel=$(field_max sixel_images)
    new_failures=$(field_max failures)
    classic=$(field_max classic_placements)
    placeholder=$(field_max placeholder_placements)
    sixel_place=$(field_max sixel_placements)

    [ "$new_kitty" -ge "$(( kitty_transfers + want_kitty ))" ] \
        || fail "$name completed $(( new_kitty - kitty_transfers )) Kitty transfers, wanted $want_kitty"
    [ "$new_sixel" -ge "$(( sixel_images + want_sixel ))" ] \
        || fail "$name decoded $(( new_sixel - sixel_images )) Sixel images, wanted $want_sixel"
    [ "$new_replies" -ge "$(( replies + want_replies ))" ] \
        || fail "$name drew $(( new_replies - replies )) PTY replies, wanted $want_replies"
    [ "$new_commands" -ge "$(( kitty_commands + want_commands ))" ] \
        || fail "$name sent $(( new_commands - kitty_commands )) Kitty commands, wanted $want_commands"
    [ "$new_failures" -le "$failures" ] \
        || fail "$name raised $(( new_failures - failures )) typed graphics failures"

    case "$want_kind" in
        classic) kind_count="$classic" ;;
        placeholder) kind_count="$placeholder" ;;
        sixel) kind_count="$sixel_place" ;;
        both) kind_count=$(( classic < sixel_place ? classic : sixel_place )) ;;
    esac
    [ "$kind_count" -ge 1 ] \
        || fail "$name left no live $want_kind placement (classic=$classic placeholder=$placeholder sixel=$sixel_place)"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" \
        "$(( new_commands - kitty_commands ))" "$(( new_kitty - kitty_transfers ))" \
        "$(( new_sixel - sixel_images ))" "$(( new_replies - replies ))" \
        "$classic" "$placeholder" "$sixel_place" "$want_kind" >>"$EVIDENCE"
    replies="$new_replies"
    kitty_commands="$new_commands"
    kitty_transfers="$new_kitty"
    sixel_images="$new_sixel"
}

#                            transfers sixel replies commands kind
run_step placeholder;   assert_step placeholder   1 0 0 1 placeholder
run_step chafa-kitty;   assert_step chafa-kitty   1 0 0 1 classic
run_step chafa-sixel;   assert_step chafa-sixel   0 1 0 0 sixel
run_step gnuplot-sixel; assert_step gnuplot-sixel 0 1 0 0 sixel
# Yazi emits nothing until its generic Kitty query is answered: the command and
# reply increments are that handshake. It then draws through Sixel, because
# Scribe truthfully advertises Sixel in DA1 and this release prefers it when
# both protocols are offered. Kitty classic display from a real application is
# Chafa's case, and Kitty placeholders are the owned fixture's.
run_step yazi;          assert_step yazi          0 1 1 1 sixel
run_step chafa-ssh;     assert_step chafa-ssh     1 1 0 1 both
run_step gnuplot-ssh;   assert_step gnuplot-ssh   0 1 0 0 sixel
run_step yazi-ssh;      assert_step yazi-ssh      0 1 1 1 sixel

python3 - "$OUT/apps.json" "$EVIDENCE" <<'PY'
import json
import subprocess
import sys

path, evidence_path = sys.argv[1:]


def version(*command):
    return subprocess.run(command, capture_output=True, text=True, check=True).stdout.splitlines()[0]


steps = {}
with open(evidence_path, encoding="utf-8") as source:
    for line in source:
        (name, commands, kitty, sixel, replies, classic, placeholder,
         sixel_place, kind) = line.rstrip().split("\t")
        steps[name] = {
            "kitty_commands": int(commands),
            "kitty_transfers": int(kitty),
            "sixel_images": int(sixel),
            "pty_replies": int(replies),
            "live_classic_placements": int(classic),
            "live_placeholder_placements": int(placeholder),
            "live_sixel_placements": int(sixel_place),
            "asserted_placement_kind": kind,
            "capture": f"{name}.png",
        }

evidence = {
    "schema": 1,
    "platform": "linux",
    "terminal_spoofing": False,
    "pinned_versions": {
        "yazi": version("yazi", "--version"),
        "chafa": version("chafa", "--version"),
        "gnuplot": version("gnuplot", "--version"),
    },
    "connection_paths": {
        "direct_pty": ["placeholder", "chafa-kitty", "chafa-sixel", "gnuplot-sixel", "yazi"],
        "ssh": ["chafa-ssh", "gnuplot-ssh", "yazi-ssh"],
    },
    "steps": steps,
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(evidence, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "PASS: pinned terminal image application corpus"
