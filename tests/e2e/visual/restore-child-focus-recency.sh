#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
# e2e-timeout: 300
# X11 oracle for focus handoff between the singleton owner and one real
# cold-restore child. No wire tap: this case restarts the disposable server.
set -e

# shellcheck source=tests/e2e/visual/relaunch-common.bash
# shellcheck disable=SC1091
. /tests/visual/relaunch-common.bash

OUT=/output/restore-child-focus-recency
CLIENT_SOCKET="${SCRIBE_RUNTIME_DIR:?the entrypoint must export SCRIBE_RUNTIME_DIR}/client.sock"
PROBE_PID=""
OWNER_PID=""
CHILD_PID=""
REPLACEMENT_PID=""
BASELINE_MS=""
LAST_ELAPSED_MS=""
mkdir -p "$OUT"

cleanup() {
    kill "$PROBE_PID" "$OWNER_PID" "$CHILD_PID" "$REPLACEMENT_PID" 2>/dev/null || true
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1" >&2
    for log in "$OUT"/*.log; do
        [ -f "$log" ] || continue
        echo "--- $(basename "$log") ---" >&2
        tail -50 "$log" >&2 || true
    done
    echo "--- server log ---" >&2
    tail -60 "$SERVER_LOG" >&2 || true
    echo "--- runtime directory ---" >&2
    ls -la "$SCRIBE_RUNTIME_DIR" >&2 2>/dev/null || true
    exit 1
}

command -v xdotool >/dev/null || fail "xdotool is missing from the visual image"
xdotool help 2>&1 | grep -q getwindowpid \
    || fail "xdotool lacks getwindowpid"
command -v xmessage >/dev/null || fail "xmessage is missing from the visual image"

client_count() { pgrep -xc scribe-client 2>/dev/null || true; }
client_count_is() { [ "$(client_count)" -eq "$1" ]; }
# X11 reparenting can make both a GPUI window and its WM frame match Scribe's
# title or class. Keep only windows whose kernel-reported owner is the client.
scribe_windows() {
    local wid pid
    { xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null || true
      xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
    } | sort -u | while read -r wid; do
        pid=$(xdotool getwindowpid "$wid" 2>/dev/null || true)
        [ -r "/proc/$pid/comm" ] || continue
        [ "$(cat "/proc/$pid/comm")" = scribe-client ] && echo "$wid"
    done
}
endpoint_count() {
    find "$SCRIBE_RUNTIME_DIR" -maxdepth 1 -type s -name 'client-focus-*.sock' 2>/dev/null | wc -l
}
endpoint_count_is() { [ "$(endpoint_count)" -eq "$1" ]; }

restore_hash() {
    find "$RESTORE_DIR" -type f ! -name bootstrap.lock -print0 2>/dev/null \
        | sort -z | xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1
}

restore_claim_count() {
    python3 - "$RESTORE_DIR/index.toml" <<'PY'
import sys, tomllib
try:
    with open(sys.argv[1], "rb") as handle:
        print(len(tomllib.load(handle).get("claimed", [])))
except (FileNotFoundError, tomllib.TOMLDecodeError):
    print(-1)
PY
}

wait_for_restore_stability() {
    local previous="" current="" matches=0 deadline=$((SECONDS + ${1:-15}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        current=$(restore_hash)
        if [ "$current" = "$previous" ]; then
            matches=$((matches + 1))
            [ "$matches" -eq 3 ] && return 0
        else
            matches=0
        fi
        previous=$current
        sleep 0.5
    done
    return 1
}

window_for_pid() {
    local wanted="$1" wid
    for wid in $(scribe_windows); do
        [ "$(xdotool getwindowpid "$wid" 2>/dev/null || true)" = "$wanted" ] && { echo "$wid"; return; }
    done
    return 1
}

all_windows_belong_to() {
    local wanted="$1" wid
    for wid in $(scribe_windows); do
        [ "$(xdotool getwindowpid "$wid" 2>/dev/null || true)" = "$wanted" ] || return 1
    done
}

write_process_log() {
    local role="$1" pid="$2" wid="$3"
    {
        echo "role=$role"
        echo "pid=$pid"
        echo "window=$wid"
        echo -n "cmdline="
        tr '\0' ' ' <"/proc/$pid/cmdline"
        echo
        echo "exe=$(readlink -f "/proc/$pid/exe")"
        sed -n '1,12p' "/proc/$pid/status"
    } >"$OUT/$role-process.log"
}

raise_probe() {
    if [ -z "$PROBE_PID" ] || ! kill -0 "$PROBE_PID" 2>/dev/null; then
        xmessage -geometry 180x70-0-0 'focus recency probe' >"$OUT/xmessage.log" 2>&1 &
        PROBE_PID=$!
        sleep 0.8
    fi
    local probe
    probe=$(xdotool search --name '[Xx]message' 2>/dev/null | tail -1)
    [ -n "$probe" ] || fail "focus probe never mapped"
    focus_window "$probe"
    window_is_active "$probe" || fail "focus probe never took activation"
}

wait_for_activation() {
    local target="$1" started_ms="$2" now_ms deadline_ms
    deadline_ms=$((started_ms + 2000))
    while :; do
        now_ms=$(date +%s%3N)
        if window_is_active "$target"; then
            LAST_ELAPSED_MS=$((now_ms - started_ms))
            return 0
        fi
        [ "$now_ms" -ge "$deadline_ms" ] && return 1
        sleep 0.05
    done
}

write_phase_json() {
    local path="$1" phase="$2" target="$3" elapsed="$4"
    shift 4
    python3 - "$path" "$phase" "$target" "$elapsed" "$BASELINE_MS" "$@" <<'PY'
import json, sys

(path, phase, target, elapsed, baseline,
 bw, bc, bh, bp, bs, br, be, b_hash,
 aw, ac, ah, ap, a_s, ar, ae, a_hash) = sys.argv[1:]

def metrics(values):
    windows, clients, hellos, ptys, sessions, claims, endpoints, restore_hash = values
    return {
        "windows": int(windows), "client_processes": int(clients),
        "hello_events": int(hellos), "pty_events": int(ptys),
        "sessions": int(sessions), "restore_claims": int(claims),
        "restore_endpoints": int(endpoints), "restore_hash": restore_hash,
    }

document = {
    "phase": phase, "target_window": int(target),
    "elapsed_ms": int(elapsed),
    "owner_baseline_ms": int(baseline or elapsed),
    "before": metrics((bw, bc, bh, bp, bs, br, be, b_hash)),
    "after": metrics((aw, ac, ah, ap, a_s, ar, ae, a_hash)),
}
with open(path, "w") as handle:
    json.dump(document, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

capture_metrics() {
    local ptys
    ptys=$(count_log "$SERVER_LOG" "created new PTY session")
    printf '%s|%s|%s|%s|%s|%s|%s|%s\n' \
        "$(scribe_windows | wc -l)" "$(client_count)" \
        "$(count_log "$SERVER_LOG" "client identified via Hello")" "$ptys" "$ptys" \
        "$(restore_claim_count)" "$(endpoint_count)" "$(restore_hash)"
}

run_handoff() {
    local phase="$1" target="$2" artifact="$3" screenshot="${4:-}"
    local before after log status=0 started_ms
    local bw bc bh bp bs br be b_hash aw ac ah ap a_s ar ae a_hash

    wait_for_restore_stability 15 || fail "$phase restore state did not settle"
    before=$(capture_metrics)
    IFS='|' read -r bw bc bh bp bs br be b_hash <<<"$before"
    raise_probe
    log="$OUT/$phase-duplicate.log"
    : >"$log"
    started_ms=$(date +%s%3N)
    timeout 5 scribe-client >"$log" 2>&1 || status=$?
    [ "$status" -eq 0 ] || fail "$phase duplicate exited $status"
    grep -qF "terminal client already running; sent focus and exiting" "$log" \
        || fail "$phase duplicate missed the singleton handoff"
    wait_for_activation "$target" "$started_ms" \
        || fail "$phase did not activate window $target within 2 seconds"
    [ "$LAST_ELAPSED_MS" -le 2000 ] || fail "$phase exceeded 2 seconds"
    if [ -n "$BASELINE_MS" ]; then
        [ "$LAST_ELAPSED_MS" -le $((BASELINE_MS + 500)) ] \
            || fail "$phase was more than 500 ms behind owner baseline"
    else
        BASELINE_MS=$LAST_ELAPSED_MS
    fi

    after=$(capture_metrics)
    IFS='|' read -r aw ac ah ap a_s ar ae a_hash <<<"$after"
    [ "$before" = "$after" ] \
        || fail "$phase duplicate changed windows, clients, Hello/PTy/session counts, restore claims, endpoints, or restore hash"
    write_phase_json "$OUT/$artifact" "$phase" "$target" "$LAST_ELAPSED_MS" \
        "$bw" "$bc" "$bh" "$bp" "$bs" "$br" "$be" "$b_hash" \
        "$aw" "$ac" "$ah" "$ap" "$a_s" "$ar" "$ae" "$a_hash"
    [ -z "$screenshot" ] || scrot -o "$OUT/$screenshot"
}

socket_owned_by() {
    local path="$1" pid="$2" inode fd
    inode=$(awk -v path="$path" '$8 == path { print $7; exit }' /proc/net/unix)
    [ -n "$inode" ] || return 1
    for fd in "/proc/$pid/fd/"*; do
        [ "$(readlink "$fd" 2>/dev/null || true)" = "socket:[$inode]" ] && return 0
    done
    return 1
}

# Create two replayable windows with the existing visual setup, then crash only
# disposable container processes so the next launch performs a cold restore.
build_two_window_owner
wait_for_restore_stability 15 || fail "two-window restore state did not settle"
[ "$(snapshot_count)" -eq 2 ] || fail "setup expected two restore snapshots"
scribe-test daemon stop >/dev/null 2>&1 || true
pkill -KILL -x scribe-client || true
wait_until 15 client_count_is 0 || fail "setup clients survived SIGKILL"
scribe-test server stop
wait_until 15 sh -c '! pgrep -x scribe-server >/dev/null' \
    || fail "setup server survived stop"
: >"$SERVER_LOG"
scribe-test server start
pgrep -x scribe-server >/dev/null || fail "replacement test server did not start"

# Auto fan-out is load-bearing: the child must be spawned by cold restore and
# carry the production --restore-child argument.
scribe-client >"$OUT/owner-child-runtime.log" 2>&1 &
OWNER_PID=$!
wait_until 30 client_count_is 2 || fail "cold restore did not create owner and child processes"
wait_until 30 window_count_is 2 || fail "cold restore did not map two windows"
wait_until 20 endpoint_count_is 1 || fail "restore child did not bind one focus endpoint"
for pid in $(pgrep -x scribe-client); do
    [ "$pid" = "$OWNER_PID" ] && continue
    if tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q -- '--restore-child'; then
        CHILD_PID=$pid
    fi
done
[ -n "$CHILD_PID" ] || fail "second client is not a real --restore-child process"
OWNER_WINDOW=$(window_for_pid "$OWNER_PID") || fail "no X11 window maps to owner PID $OWNER_PID"
CHILD_WINDOW=$(window_for_pid "$CHILD_PID") || fail "no X11 window maps to child PID $CHILD_PID"
[ "$OWNER_WINDOW" != "$CHILD_WINDOW" ] || fail "owner and child mapped to the same X11 window"
write_process_log owner "$OWNER_PID" "$OWNER_WINDOW"
write_process_log child "$CHILD_PID" "$CHILD_WINDOW"
wait_for_restore_stability 20 || fail "restored topology did not settle"
[ "$(count_log "$SERVER_LOG" "client identified via Hello")" -eq 2 ] \
    || fail "restored topology did not produce exactly two Hello events"
[ "$(count_log "$SERVER_LOG" "created new PTY session")" -eq 2 ] \
    || fail "restored topology did not produce exactly two PTYs/sessions"
printf '%s  restore-state\n' "$(restore_hash)" >"$OUT/restore-before.sha256"

# @lat: [[test#Test Harness#Visual E2E Tests#Client relaunch handling#Restore-child focus recency crosses processes]]
focus_window "$OWNER_WINDOW"
run_handoff owner-baseline "$OWNER_WINDOW" owner-baseline.json
focus_window "$CHILD_WINDOW"
run_handoff child-handoff "$CHILD_WINDOW" child-handoff.json child-selection.png
focus_window "$OWNER_WINDOW"
run_handoff owner-return "$OWNER_WINDOW" owner-return.json

# Record child recency, then leave its endpoint inode behind with SIGKILL. The
# next duplicate must prune it and activate the owner in the same handoff.
focus_window "$CHILD_WINDOW"
sleep 0.5
kill -KILL "$CHILD_PID"
wait_until 15 client_count_is 1 || fail "stale child process did not exit"
wait_until 15 window_count_is 1 || fail "stale child window did not disappear"
[ "$(endpoint_count)" -eq 1 ] || fail "stale child left no endpoint debris"
run_handoff stale-fallback "$OWNER_WINDOW" stale-fallback.json owner-fallback.png
printf '%s  restore-state\n' "$(restore_hash)" >"$OUT/restore-after.sha256"
cmp -s "$OUT/restore-before.sha256" "$OUT/restore-after.sha256" \
    || fail "duplicate phases changed restore state"

# Updater-shaped replacement: crash the last old owner, preserve client.sock
# plus dead endpoint debris, then launch one bare replacement.
UPDATER_HELLOS_BEFORE=$(count_log "$SERVER_LOG" "client identified via Hello")
UPDATER_PTYS_BEFORE=$(count_log "$SERVER_LOG" "created new PTY session")
UPDATER_CLAIMS_BEFORE=$(restore_claim_count)
DEBRIS_BEFORE=$(endpoint_count)
kill -KILL "$OWNER_PID"
wait_until 15 client_count_is 0 || fail "old owner survived updater crash"
wait_until 15 window_count_is 0 || fail "old owner window survived updater crash"
[ -S "$CLIENT_SOCKET" ] || fail "updater setup lost stale client.sock"
[ "$DEBRIS_BEFORE" -gt 0 ] || fail "updater setup has no strict-prefix debris"

scribe-client >"$OUT/updater-owner-runtime.log" 2>&1 &
REPLACEMENT_PID=$!
wait_until 30 client_count_is 1 || fail "bare updater replacement did not become sole owner"
wait_until 30 window_count_is 2 || fail "bare updater replacement did not reclaim both server windows"
wait_until 5 endpoint_count_is 0 || fail "replacement did not remove dead strict-prefix debris"
socket_owned_by "$CLIENT_SOCKET" "$REPLACEMENT_PID" \
    || fail "replacement PID does not own reclaimed client.sock"
all_windows_belong_to "$REPLACEMENT_PID" \
    || fail "replacement windows belong to more than one GPUI process"
[ "$(count_log "$SERVER_LOG" "created new PTY session")" -eq "$UPDATER_PTYS_BEFORE" ] \
    || fail "updater replacement duplicated a PTY/session"
[ "$(count_log "$SERVER_LOG" "client identified via Hello")" -eq $((UPDATER_HELLOS_BEFORE + 2)) ] \
    || fail "updater replacement did not attach exactly once per reclaimed window"
wait_for_restore_stability 15 || fail "updater restore state did not settle"
[ "$(restore_claim_count)" -eq "$UPDATER_CLAIMS_BEFORE" ] \
    || fail "updater replacement left an extra restore claim"
write_process_log updater-owner "$REPLACEMENT_PID" "$(scribe_windows | paste -sd, -)"

python3 - "$OUT/updater-reclaim.json" "$REPLACEMENT_PID" "$DEBRIS_BEFORE" \
    "$UPDATER_HELLOS_BEFORE" "$(count_log "$SERVER_LOG" "client identified via Hello")" \
    "$UPDATER_PTYS_BEFORE" "$(count_log "$SERVER_LOG" "created new PTY session")" \
    "$UPDATER_CLAIMS_BEFORE" "$(restore_claim_count)" <<'PY'
import json, sys
path, pid, debris, hb, ha, pb, pa, cb, ca = sys.argv[1:]
with open(path, "w") as handle:
    json.dump({
        "replacement_pid": int(pid), "client_processes": 1, "windows": 2,
        "client_socket_reclaimed": True,
        "dead_endpoints_before": int(debris), "dead_endpoints_after": 0,
        "hello_events_before": int(hb), "hello_events_after": int(ha),
        "pty_events_before": int(pb), "pty_events_after": int(pa),
        "sessions_before": int(pb), "sessions_after": int(pa),
        "restore_claims_before": int(cb), "restore_claims_after": int(ca),
    }, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "PASS: restore-child focus recency, stale fallback, and updater reclaim"
