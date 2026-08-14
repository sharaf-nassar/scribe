# Shared helpers for the terminal-client relaunch visual E2Es.

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
RESTORE_DIR="$STATE_DIR/restore"

fail() {
    echo "FAIL: $1" >&2
    for log in /output/relaunch-duplicate.log /output/refused-claim-client.log; do
        [ -f "$log" ] || continue
        echo "--- $(basename "$log") ---" >&2
        cat "$log" >&2 || true
    done
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    echo "--- server log tail ---" >&2
    tail -40 "$SERVER_LOG" >&2 || true
    echo "--- restore store ---" >&2
    find "$RESTORE_DIR" -maxdepth 2 -type f -print >&2 2>/dev/null || true
    exit 1
}

scribe_windows() {
    { xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null || true
      xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
    } | sort -u
}

window_count_is() { [ "$(scribe_windows | wc -l)" -eq "$1" ]; }
window_is_active() { [ "$(xdotool getactivewindow 2>/dev/null)" = "$1" ]; }

wait_until() {
    local timeout_secs="$1" started
    shift
    started=$(date +%s)
    until "$@"; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.2
    done
}

count_log() { grep -cF "$2" "$1" 2>/dev/null || true; }

wait_for_log_growth() {
    local path="$1" needle="$2" baseline="$3" deadline=$((SECONDS + ${4:-20}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ "$(count_log "$path" "$needle")" -gt "$baseline" ] && return 0
        sleep 0.2
    done
    return 1
}

count_frames() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys

path, direction, wanted = sys.argv[1:]
total = 0
try:
    handle = open(path)
except OSError:
    print(0)
    raise SystemExit
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == direction and message.get("type") == wanted:
            total += 1
print(total)
PY
}

latest_window_state() {
    python3 - "$RECORD" <<'PY'
import json, sys

found = None
try:
    handle = open(sys.argv[1])
except OSError:
    raise SystemExit(1)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == "server" and message.get("type") == "WindowList":
            found = sorted(
                (
                    window["window_id"],
                    window["session_count"],
                    window["connected"],
                )
                for window in message["windows"]
            )
if found is None:
    raise SystemExit(1)
print(json.dumps(found, separators=(",", ":")))
PY
}

window_state_counts() {
    python3 - "$1" <<'PY'
import json, sys

windows = json.loads(sys.argv[1])
print(len(windows), sum(window[1] for window in windows))
PY
}

wait_for_window_lists() {
    local baseline="$1" deadline=$((SECONDS + ${2:-20}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ "$(count_frames server WindowList)" -gt "$baseline" ] && return 0
        sleep 0.2
    done
    return 1
}

focus_window() {
    xdotool windowactivate --sync "$1" 2>/dev/null \
        || xdotool windowfocus --sync "$1" 2>/dev/null || true
    sleep 0.8
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

snapshot_count() {
    find "$RESTORE_DIR/windows" -maxdepth 1 -type f -name '*.toml' 2>/dev/null | wc -l
}

snapshot_count_is() { [ "$(snapshot_count)" -eq "$1" ]; }

# Build a live owner with two mapped windows and replayable snapshots. Both
# tests need multiple live sessions; the refused-claim case also needs a second
# index entry so erroneous restore-child fan-out is observable.
build_two_window_owner() {
    sleep 1.0
    window_count_is 1 || fail "setup expected exactly one entrypoint Scribe window"
    local first new_windows_before
    first=$(scribe_windows | head -1)
    focus_window "$first"
    new_windows_before=$(count_log "$CLIENT_LOG" "opened a new terminal window")
    send_keys ctrl+shift+n
    wait_for_log_growth "$CLIENT_LOG" "opened a new terminal window" "$new_windows_before" 20 \
        || fail "setup could not open the second live window"
    wait_until 20 window_count_is 2 || fail "setup second window never mapped"
    wait_until 20 snapshot_count_is 2 || fail "setup did not persist two snapshots"
    [ -f "$RESTORE_DIR/index.toml" ] || fail "setup wrote no restore index"
}
