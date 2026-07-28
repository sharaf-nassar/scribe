#!/bin/bash
# Offline regression tests for the Debian postinst maintainer script.
#
# Sources only the variable + function definitions from `dist/debian/postinst`
# and exercises individual functions against fixtures (e.g. a real zombie
# child process) without running the full installer or touching the live
# user session.

set -u

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
postinst="${repo_root}/dist/debian/postinst"

if [ ! -r "$postinst" ]; then
    echo "FAIL: postinst not found at ${postinst}" >&2
    exit 2
fi

# Strip everything from the SERVER_RUNTIME_GENERATION assignment onward — the
# case statement and trailing `exit 0` would otherwise terminate this test
# process when sourced.
eval "$(awk '/^SERVER_RUNTIME_GENERATION="\$\(compute_server_runtime_generation\)"$/{exit} {print}' "$postinst")"
set +e  # postinst sets -e; tests need to inspect non-zero return codes

failures=0
LAUNCHER_PIDS=()

cleanup() {
    local pid
    for pid in "${LAUNCHER_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null
    done
    wait 2>/dev/null
}
trap cleanup EXIT

# spawn_zombie prints the PID of a zombie process to stdout. bash auto-reaps
# its own backgrounded children, so we drive the fork from python: the python
# launcher forks a child that exits immediately, then sleeps to keep the
# zombie alive. The launcher PID is recorded in LAUNCHER_PIDS for cleanup.
spawn_zombie() {
    local fifo
    fifo=$(mktemp -u)
    mkfifo "$fifo"
    python3 -u -c '
import os, time
pid = os.fork()
if pid == 0:
    os._exit(0)
print(pid)
time.sleep(60)
' > "$fifo" &
    local py_pid=$!
    LAUNCHER_PIDS+=("$py_pid")
    local zombie_pid
    read -r zombie_pid < "$fifo"
    rm -f "$fifo"

    local tries=0
    while [ "$tries" -lt 100 ]; do
        local state=""
        state=$(awk '/^State:/ {print $2; exit}' "/proc/$zombie_pid/status" 2>/dev/null || true)
        if [ "$state" = "Z" ]; then
            printf '%s\n' "$zombie_pid"
            return 0
        fi
        sleep 0.05
        tries=$((tries + 1))
    done
    echo "spawn_zombie: PID $zombie_pid never entered zombie state" >&2
    return 1
}

if command -v python3 >/dev/null 2>&1; then
    # ── wait_for_pid_exit treats a zombie as exited ──────────────────────
    zpid=$(spawn_zombie) || exit 1
    if wait_for_pid_exit "$zpid" 5; then
        echo "PASS: wait_for_pid_exit treats zombie PID $zpid as exited"
    else
        echo "FAIL: wait_for_pid_exit blocked on zombie PID $zpid (kill -0 lies on zombies)"
        failures=$((failures + 1))
    fi

    # ── stop_client_processes does not block client relaunch on a zombie ─
    zpid=$(spawn_zombie) || exit 1
    if stop_client_processes "$zpid" >/dev/null 2>&1; then
        echo "PASS: stop_client_processes succeeded with zombie PID $zpid"
    else
        echo "FAIL: stop_client_processes returned non-zero for zombie PID $zpid"
        failures=$((failures + 1))
    fi
else
    echo "SKIP: zombie regressions require python3"
fi

# ── Vulkan guard restores the preinst stash without touching a session ───
probe_fixture=$(mktemp -d)
sleep 60 &
session_pid=$!
LAUNCHER_PIDS+=("$session_pid")
STATE_DIR="$probe_fixture/state"
CLIENT_BIN_PATH="$probe_fixture/scribe-client"
APP_DISPLAY_NAME="Scribe"
mkdir -p "$STATE_DIR"
cat > "$CLIENT_BIN_PATH" <<'EOF'
#!/bin/sh
[ "$1" = "--vulkan-probe" ] && exit 1
exit 0
EOF
cat > "${STATE_DIR}/upgrade-client-binary" <<'EOF'
#!/bin/sh
[ "$1" = "--vulkan-probe" ] && exit 0
exit 0
EOF
chmod +x "$CLIENT_BIN_PATH" "${STATE_DIR}/upgrade-client-binary"

guard_output="$probe_fixture/guard-output"
if probe_client_vulkan >"$guard_output" 2>&1; then
    echo "FAIL: Vulkan guard accepted a failed probe"
    failures=$((failures + 1))
elif [ "$CLIENT_VULKAN_READY" != "0" ]; then
    echo "FAIL: Vulkan guard left client relaunch enabled"
    failures=$((failures + 1))
elif ! "$CLIENT_BIN_PATH" --vulkan-probe; then
    echo "FAIL: Vulkan guard did not restore the stashed client"
    failures=$((failures + 1))
elif ! kill -0 "$session_pid" 2>/dev/null; then
    echo "FAIL: Vulkan guard disturbed the running session fixture"
    failures=$((failures + 1))
elif ! grep -q 'Vulkan probe failed' "$guard_output" || \
     ! grep -q 'Restored the previous client' "$guard_output"; then
    echo "FAIL: Vulkan guard did not surface the restore warning"
    failures=$((failures + 1))
else
    echo "PASS: Vulkan guard restored the client and left sessions alive"
fi
rm -rf "$probe_fixture"

if [ "$failures" -gt 0 ]; then
    echo "${failures} postinst regression test(s) failed."
    exit 1
fi
echo "All postinst regression tests passed."
exit 0
