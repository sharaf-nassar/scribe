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

# ── A handoff-ready successor retires a captured predecessor ─────────────
sleep 60 &
retired_pid=$!
LAUNCHER_PIDS+=("$retired_pid")
retired_start="$(awk '{print $22}' "/proc/$retired_pid/stat")"
retired_hash="$(sha256sum "/proc/$retired_pid/exe" | cut -d' ' -f1)"
retired_record="${retired_pid}:${retired_start}:${retired_hash}"
if ! wait_for_server_process_records_exit 1 "$retired_record"; then
    echo "FAIL: captured predecessor PID $retired_pid survived the bounded retirement"
    failures=$((failures + 1))
elif server_process_record_is_alive "$retired_record"; then
    echo "FAIL: bounded retirement returned while predecessor PID $retired_pid was alive"
    failures=$((failures + 1))
else
    wait "$retired_pid" 2>/dev/null || true
    echo "PASS: bounded retirement terminates a captured predecessor"
fi

# A PID and birth time are insufficient authorization to signal after exec.
# The captured executable hash must still match at the moment of retirement.
sleep 60 &
reused_pid=$!
LAUNCHER_PIDS+=("$reused_pid")
reused_start="$(awk '{print $22}' "/proc/$reused_pid/stat")"
reused_record="${reused_pid}:${reused_start}:not-the-running-executable"
if wait_for_server_process_records_exit 1 "$reused_record"; then
    echo "FAIL: bounded retirement accepted a mismatched executable identity"
    failures=$((failures + 1))
elif ! kill -0 "$reused_pid" 2>/dev/null; then
    echo "FAIL: bounded retirement signalled a mismatched executable identity"
    failures=$((failures + 1))
else
    echo "PASS: bounded retirement spares a mismatched executable identity"
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

# ── Upgrade log survives the readiness check and post-upgrade cleanup ────
upgrade_fixture=$(mktemp -d)
PRIVILEGED_USER_UID=""
TARGET_UID="$(id -u)"
STATE_DIR="$upgrade_fixture/state"
HOT_RELOAD_READY_TIMEOUT_SECS=10
mkdir -p "$STATE_DIR"
fake_server="$upgrade_fixture/scribe-server"
cat > "$fake_server" <<'EOF'
#!/bin/sh
echo "IPC server listening"
# Keep writing after handoff like a real successor server, so a log that is
# unlinked after the readiness check grows into an invisible tmpfs inode.
while :; do
    echo "post-handoff output"
    sleep 1
done
EOF
chmod +x "$fake_server"

UPGRADE_PID=""
if ! spawn_upgrade_server "$fake_server"; then
    echo "FAIL: spawn_upgrade_server never observed the bind-ready log line"
    failures=$((failures + 1))
else
    LAUNCHER_PIDS+=("$UPGRADE_PID")
    cleanup_upgrade_state
    # readlink appends " (deleted)" for an unlinked target, so this single
    # comparison catches both a moved log and a log removed underneath the
    # still-running successor.
    server_stdout=$(readlink "/proc/${UPGRADE_PID}/fd/1" 2>/dev/null || true)
    if [ "$server_stdout" != "${STATE_DIR}/upgrade.log" ]; then
        echo "FAIL: upgrade server stdout points at '${server_stdout}', not the state-dir log"
        failures=$((failures + 1))
    elif [ ! -f "${STATE_DIR}/upgrade.log" ]; then
        echo "FAIL: upgrade log was removed while the server still writes to it"
        failures=$((failures + 1))
    else
        echo "PASS: upgrade server keeps a live state-dir log after cleanup"
    fi
fi
kill "$UPGRADE_PID" 2>/dev/null
rm -rf "$upgrade_fixture"

# ── AI SessionEnd hooks install and clear without payload parsing ────────
if command -v python3 >/dev/null 2>&1; then
    hook_fixture=$(mktemp -d)
    hook_home="$hook_fixture/home"
    hook_capture="$hook_fixture/helper-argv"
    python_capture="$hook_fixture/python-called"
    mkdir -p "$hook_home/.claude" "$hook_home/.codex" "$hook_fixture/bin"

    if ! HOME="$hook_home" SCRIBE_INSTALL_PREFIX="$repo_root/dist" \
        bash "$repo_root/dist/setup-claude-hooks.sh" >/dev/null 2>&1; then
        echo "FAIL: Claude SessionEnd hook setup failed"
        failures=$((failures + 1))
    elif ! HOME="$hook_home" \
        bash "$repo_root/dist/setup-codex-hooks.sh" \
            --hook-source "$repo_root/dist" >/dev/null 2>&1; then
        echo "FAIL: Codex SessionEnd hook setup failed"
        failures=$((failures + 1))
    elif ! python3 - "$repo_root" "$hook_home" <<'PY'
import json
import os
import re
import sys

root, home = sys.argv[1:]
dist = os.path.join(root, "dist")

with open(os.path.join(home, ".claude", "settings.json")) as handle:
    claude = json.load(handle)
claude_entry = claude["hooks"]["SessionEnd"]
assert claude_entry == [{
    "hooks": [{
        "type": "command",
        "command": os.path.join(dist, "ai-hook-claude.sh") + " session_end",
    }],
}]

hooks_path = os.path.join(home, ".codex", "hooks.json")
with open(hooks_path) as handle:
    codex = json.load(handle)
codex_entry = codex["hooks"]["SessionEnd"]
assert codex_entry == [{
    "hooks": [{
        "type": "command",
        "command": '"' + os.path.join(dist, "ai-hook-codex.sh") + '" session_end',
        "timeout": 3,
    }],
}]

with open(os.path.join(home, ".codex", "config.toml")) as handle:
    config = handle.read()
trust_key = hooks_path + ":session_end:0:0"
header = "[hooks.state." + json.dumps(trust_key) + "]"
block = config.split(header, 1)[1].split("\n[", 1)[0]
assert re.search(r"^enabled = true$", block, re.MULTILINE)
assert re.search(r'^trusted_hash = "sha256:[0-9a-f]{64}"$', block, re.MULTILINE)
PY
    then
        echo "FAIL: SessionEnd hook registrations or Codex trust state are wrong"
        failures=$((failures + 1))
    else
        echo "PASS: Claude and Codex install unmatched trusted SessionEnd hooks"
    fi

    cat > "$hook_fixture/bin/python3" <<'EOF'
#!/bin/sh
: >"$SCRIBE_PYTHON_CAPTURE"
exit 1
EOF
    cat > "$hook_fixture/helper" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"$SCRIBE_HOOK_CAPTURE"
cat >/dev/null
EOF
    chmod +x "$hook_fixture/bin/python3" "$hook_fixture/helper"

    printf '{not-json' | PATH="$hook_fixture/bin:/usr/bin:/bin" \
        SCRIBE_HOOK_HELPER="$hook_fixture/helper" \
        SCRIBE_HOOK_CAPTURE="$hook_capture" \
        SCRIBE_PYTHON_CAPTURE="$python_capture" \
        sh "$repo_root/dist/ai-hook-claude.sh" session_end
    printf '{not-json' | PATH="$hook_fixture/bin:/usr/bin:/bin" \
        SCRIBE_HOOK_HELPER="$hook_fixture/helper" \
        SCRIBE_HOOK_CAPTURE="$hook_capture" \
        SCRIBE_PYTHON_CAPTURE="$python_capture" \
        sh "$repo_root/dist/ai-hook-codex.sh" session_end

    if [ -e "$python_capture" ]; then
        echo "FAIL: SessionEnd adapter started a JSON interpreter"
        failures=$((failures + 1))
    elif [ "$(wc -l < "$hook_capture")" -ne 2 ] || \
         ! grep -Fxq -- '--provider=claude_code --event=state_cleared' "$hook_capture" || \
         ! grep -Fxq -- '--provider=codex_code --event=state_cleared' "$hook_capture"; then
        echo "FAIL: SessionEnd adapters did not emit provider state_cleared events"
        failures=$((failures + 1))
    else
        echo "PASS: SessionEnd adapters clear both providers without parsing payloads"
    fi
    rm -rf "$hook_fixture"
else
    echo "SKIP: AI SessionEnd hook regressions require python3"
fi

# ── Agent skill install: fresh, idempotent, regenerating, foreign-safe ───
if command -v python3 >/dev/null 2>&1; then
    skill_fixture=$(mktemp -d)
    skill_home="$skill_fixture/home"
    # A macOS-bundle-shaped prefix makes CLI resolution hermetic: the setup
    # scripts look for Contents/MacOS/scribe next to the Resources prefix
    # before any machine-wide /usr/bin path.
    skill_resources="$skill_fixture/Contents/Resources"
    skill_macos="$skill_fixture/Contents/MacOS"
    mkdir -p "$skill_home/.claude" "$skill_home/.codex" "$skill_resources" "$skill_macos"

    cat > "$skill_macos/scribe" <<'EOF'
#!/bin/sh
if [ "$1" = "agent" ] && [ "$2" = "skill" ]; then
    printf '# Scribe agent control\n\nbody %s\n' "${SCRIBE_FAKE_SKILL_REVISION:-r1}"
    exit 0
fi
echo "unexpected argv: $*" >&2
exit 2
EOF
    chmod +x "$skill_macos/scribe"

    skill_failures=0
    for provider in claude codex; do
        setup_script="$repo_root/dist/setup-${provider}-hooks.sh"
        target="$skill_home/.${provider}/skills/scribe-terminal/SKILL.md"

        if ! HOME="$skill_home" SCRIBE_INSTALL_PREFIX="$skill_resources" \
            bash "$setup_script" >/dev/null 2>&1; then
            echo "FAIL: ${provider} setup with agent skill install failed"
            skill_failures=$((skill_failures + 1))
            continue
        fi
        if [ ! -f "$target" ] || \
           ! grep -q "SCRIBE-MANAGED-SKILL" "$target" || \
           ! grep -q "^name: scribe-terminal$" "$target" || \
           ! grep -q "^body r1$" "$target"; then
            echo "FAIL: ${provider} fresh install did not write the marked generated skill"
            skill_failures=$((skill_failures + 1))
            continue
        fi

        before=$(stat -c '%i %Y' "$target")
        rerun_output=$(HOME="$skill_home" SCRIBE_INSTALL_PREFIX="$skill_resources" \
            bash "$setup_script" 2>&1)
        after=$(stat -c '%i %Y' "$target")
        if [ "$before" != "$after" ] || \
           ! printf '%s' "$rerun_output" | grep -q "skills/scribe-terminal/SKILL.md already up to date"; then
            echo "FAIL: ${provider} rerun with unchanged output rewrote the skill file"
            skill_failures=$((skill_failures + 1))
            continue
        fi

        if ! HOME="$skill_home" SCRIBE_INSTALL_PREFIX="$skill_resources" \
            SCRIBE_FAKE_SKILL_REVISION=r2 bash "$setup_script" >/dev/null 2>&1 || \
           ! grep -q "^body r2$" "$target"; then
            echo "FAIL: ${provider} changed CLI output was not regenerated"
            skill_failures=$((skill_failures + 1))
            continue
        fi

        printf 'my own skill\n' > "$target"
        foreign_output=$(HOME="$skill_home" SCRIBE_INSTALL_PREFIX="$skill_resources" \
            bash "$setup_script" 2>&1)
        foreign_status=$?
        if [ "$foreign_status" -ne 0 ] || \
           [ "$(cat "$target")" != "my own skill" ] || \
           ! printf '%s' "$foreign_output" | grep -q "was not installed by Scribe"; then
            echo "FAIL: ${provider} foreign skill file was not reported and left untouched"
            skill_failures=$((skill_failures + 1))
            continue
        fi

        echo "PASS: ${provider} agent skill installs fresh, idempotently, and refuses foreign files"
    done
    failures=$((failures + skill_failures))
    rm -rf "$skill_fixture"
else
    echo "SKIP: agent skill install regressions require python3"
fi

# ── pi_integration_enabled defaults on and only reads the [terminal] key ─
pi_fixture=$(mktemp -d)
pi_config="$pi_fixture/config.toml"

if pi_integration_enabled "$pi_fixture/does-not-exist.toml"; then
    echo "PASS: pi_integration_enabled defaults to enabled when the config file is missing"
else
    echo "FAIL: pi_integration_enabled treated a missing config file as disabled"
    failures=$((failures + 1))
fi

printf '[terminal]\npi_integration = false\n' > "$pi_config"
if pi_integration_enabled "$pi_config"; then
    echo "FAIL: pi_integration_enabled ignored an explicit false under [terminal]"
    failures=$((failures + 1))
else
    echo "PASS: pi_integration_enabled honors an explicit false under [terminal]"
fi

printf '[terminal]\npi_integration = true\n' > "$pi_config"
if pi_integration_enabled "$pi_config"; then
    echo "PASS: pi_integration_enabled honors an explicit true under [terminal]"
else
    echo "FAIL: pi_integration_enabled ignored an explicit true under [terminal]"
    failures=$((failures + 1))
fi

# A key of the same name outside [terminal] must not gate setup — only the
# [terminal] section's own pi_integration key is load-bearing.
printf '[other]\npi_integration = false\n[terminal]\nclaude_code_integration = true\n' > "$pi_config"
if pi_integration_enabled "$pi_config"; then
    echo "PASS: pi_integration_enabled ignores a same-named key outside [terminal]"
else
    echo "FAIL: pi_integration_enabled leaked state from an unrelated section"
    failures=$((failures + 1))
fi
rm -rf "$pi_fixture"

if ai_setup_target_known 0 ""; then
    echo "FAIL: AI setup treated an ambiguous root package invocation as user-scoped"
    failures=$((failures + 1))
elif ! ai_setup_target_known 0 1000; then
    echo "FAIL: AI setup rejected a root invocation with a known sudo/pkexec user"
    failures=$((failures + 1))
elif ! ai_setup_target_known 1000 ""; then
    echo "FAIL: AI setup rejected a direct non-root invocation"
    failures=$((failures + 1))
else
    echo "PASS: AI setup defers ambiguous root installs and accepts known users"
fi

# ── setup-pi-extension.sh installs, repairs, and never clobbers unowned files
setup_pi="$repo_root/dist/setup-pi-extension.sh"
pi_extension_fixture=$(mktemp -d)
pi_home="$pi_extension_fixture/home"
mkdir -p "$pi_home/.pi/agent"
pi_settings="$pi_home/.pi/agent/settings.json"
printf '{"theme":"user-owned"}\n' > "$pi_settings"
target="$pi_home/.pi/agent/extensions/scribe-ai-integration.ts"

if ! HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >/dev/null 2>&1; then
    echo "FAIL: setup-pi-extension.sh failed on a fresh install"
    failures=$((failures + 1))
elif ! cmp -s "$target" "$repo_root/dist/pi-extension.ts"; then
    echo "FAIL: setup-pi-extension.sh did not install the packaged extension content"
    failures=$((failures + 1))
elif [ "$(stat -c %a "$target")" != "644" ]; then
    echo "FAIL: setup-pi-extension.sh installed unexpected permissions $(stat -c %a "$target")"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh creates the marked extension on a fresh install"
fi

# Plant an unrelated sibling extension that every later step must preserve.
sibling="$pi_home/.pi/agent/extensions/unrelated-tool.ts"
printf '// some other extension\n' > "$sibling"

before_inode="$(stat -c %i "$target" 2>/dev/null || true)"
rerun_log="$pi_extension_fixture/rerun.log"
if ! HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >"$rerun_log" 2>&1; then
    echo "FAIL: setup-pi-extension.sh failed on an identical rerun"
    failures=$((failures + 1))
elif [ "$(stat -c %i "$target" 2>/dev/null || true)" != "$before_inode" ]; then
    echo "FAIL: setup-pi-extension.sh rewrote the file when content was unchanged"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh is a no-op when content is already current"
fi

printf '// SCRIBE-MANAGED-PI-EXTENSION\n// stale content from an older release\n' > "$target"
before_inode="$(stat -c %i "$target")"
if ! HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >/dev/null 2>&1; then
    echo "FAIL: setup-pi-extension.sh failed to repair stale managed content"
    failures=$((failures + 1))
elif ! cmp -s "$target" "$repo_root/dist/pi-extension.ts"; then
    echo "FAIL: setup-pi-extension.sh left stale managed content in place"
    failures=$((failures + 1))
elif [ "$(stat -c %i "$target")" = "$before_inode" ]; then
    echo "FAIL: setup-pi-extension.sh rewrote stale content in place instead of a temp+rename swap"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh atomically replaces stale managed content"
fi

printf 'not a scribe extension\n' > "$target"
collision_log="$pi_extension_fixture/collision.log"
if HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >/dev/null 2>"$collision_log"; then
    echo "FAIL: setup-pi-extension.sh overwrote an unmarked collision"
    failures=$((failures + 1))
elif ! grep -q "not a scribe extension" "$target"; then
    echo "FAIL: setup-pi-extension.sh modified the unmarked collision target"
    failures=$((failures + 1))
elif [ ! -s "$collision_log" ]; then
    echo "FAIL: setup-pi-extension.sh refused the collision without a readable notice"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh refuses to overwrite an unmarked collision and reports it"
fi

rm -f "$target"
ln -s "$pi_extension_fixture/missing-target" "$target"
if HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >/dev/null 2>&1; then
    echo "FAIL: setup-pi-extension.sh replaced a dangling symlink collision"
    failures=$((failures + 1))
elif [ ! -L "$target" ]; then
    echo "FAIL: setup-pi-extension.sh modified a dangling symlink collision"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh refuses dangling symlink collisions"
fi

rm -f "$target"
mkdir "$target"
if HOME="$pi_home" bash "$setup_pi" --extension-source "$repo_root/dist" >/dev/null 2>&1; then
    echo "FAIL: setup-pi-extension.sh replaced a non-regular collision"
    failures=$((failures + 1))
elif [ ! -d "$target" ]; then
    echo "FAIL: setup-pi-extension.sh modified a non-regular collision"
    failures=$((failures + 1))
else
    echo "PASS: setup-pi-extension.sh refuses non-regular collisions without reading them"
fi

if ! grep -q "some other extension" "$sibling"; then
    echo "FAIL: an unrelated extension file was disturbed by Pi extension setup"
    failures=$((failures + 1))
elif ! grep -q '"theme":"user-owned"' "$pi_settings"; then
    echo "FAIL: Pi settings.json was modified by extension setup"
    failures=$((failures + 1))
else
    echo "PASS: unrelated Pi extensions and settings survive setup"
fi
rm -rf "$pi_extension_fixture"

# ── stable/dev Debian and macOS packages carry the agent CLI ─────────────
deb_manifest="$repo_root/crates/scribe-server/Cargo.toml"
macos_builder="$repo_root/dist/macos/build-dmg.sh"
macos_signer="$repo_root/dist/ci/sign-notarize-macos.sh"
release_workflow="$repo_root/.github/workflows/release.yml"
if [ "$(grep -Fc '"target/release/scribe-cli"' "$deb_manifest")" -ne 2 ] || \
    ! grep -Fq '"usr/bin/scribe"' "$deb_manifest" || \
    ! grep -Fq '"usr/bin/scribe-dev-cli"' "$deb_manifest"; then
    echo "FAIL: stable/dev Debian asset manifests omit isolated agent CLIs"
    failures=$((failures + 1))
elif ! grep -Fq 'for bin in scribe-client scribe-server scribe-cli; do' "$macos_builder" || \
    ! grep -Fq "cp \"\${BUILD_DIR}/scribe-cli\"      \"\${MACOS_DIR}/scribe\"" "$macos_builder" || \
    ! grep -Fq "for executable in \"\${APP_BUNDLE}/Contents/MacOS/\"*; do" "$macos_signer"; then
    echo "FAIL: macOS bundle does not stage and sign the agent CLI"
    failures=$((failures + 1))
elif ! grep -Fq "cp target/\${{ matrix.target }}/release/scribe-cli target/release/" "$release_workflow"; then
    echo "FAIL: macOS release staging omits scribe-cli"
    failures=$((failures + 1))
elif grep -Eq 'SKILL\.md|/skills/' "$deb_manifest" "$macos_builder"; then
    echo "FAIL: package assets include a generated skill file"
    failures=$((failures + 1))
else
    echo "PASS: stable/dev Debian and macOS package definitions include only the agent CLI"
fi

# ── stable/dev Debian and macOS packages carry flavor-neutral Pi assets ─
if [ "$(grep -Fc '"../../dist/pi-extension.ts"' "$deb_manifest")" -ne 2 ] || \
    ! grep -Fq '"usr/share/scribe/pi-extension.ts"' "$deb_manifest" || \
    ! grep -Fq '"usr/share/scribe-dev/pi-extension.ts"' "$deb_manifest" || \
    [ "$(grep -Fc '"../../dist/setup-pi-extension.sh"' "$deb_manifest")" -ne 2 ] || \
    ! grep -Fq '"usr/share/scribe/setup-pi-extension.sh"' "$deb_manifest" || \
    ! grep -Fq '"usr/share/scribe-dev/setup-pi-extension.sh"' "$deb_manifest"; then
    echo "FAIL: stable/dev Debian asset manifests omit Pi integration files"
    failures=$((failures + 1))
elif ! grep -Fq 'cp "${DIST_DIR}/pi-extension.ts"' "$macos_builder" || \
    ! grep -Fq 'cp "${DIST_DIR}/setup-pi-extension.sh"' "$macos_builder" || \
    ! grep -Fq '"${RESOURCES_DIR}/setup-pi-extension.sh"' "$macos_builder"; then
    echo "FAIL: macOS bundle assembly omits Pi integration files or setup mode"
    failures=$((failures + 1))
else
    echo "PASS: stable/dev Debian and macOS package definitions include Pi assets"
fi

# ── Beads diagnostic resolves the target user's home, not root HOME ──
beads_fixture=$(mktemp -d)
beads_target_home="$beads_fixture/target-home"
mkdir -p "$beads_target_home/.local/bin"
printf '#!/bin/sh\n: > %q\n' \
    "$beads_fixture/bd-executed" > "$beads_target_home/.local/bin/bd"
chmod +x "$beads_target_home/.local/bin/bd"

getent() {
    [ "$1" = "passwd" ] && [ "$2" = "$TARGET_UID" ] || return 2
    printf 'target:x:%s:%s::%s:/bin/sh\n' \
        "$TARGET_UID" "$TARGET_UID" "$beads_target_home"
}

user_manager_env() {
    [ "$1" = "PATH" ] || return 1
    printf '/usr/bin:/bin\n'
}

TARGET_UID=4242
saved_path="$PATH"
PATH="/usr/bin:/bin"
beads_output="$(diagnose_beads_cli 2>&1)"
beads_status=$?
PATH="$saved_path"

if [ "$beads_status" -ne 0 ]; then
    echo "FAIL: Beads diagnostic failed package configuration"
    failures=$((failures + 1))
elif ! grep -Fq "$beads_target_home/.local/bin/bd" <<< "$beads_output"; then
    echo "FAIL: Beads diagnostic missed the target user's ~/.local/bin/bd"
    failures=$((failures + 1))
elif [ -e "$beads_fixture/bd-executed" ]; then
    echo "FAIL: Beads diagnostic executed bd instead of inspecting it"
    failures=$((failures + 1))
else
    echo "PASS: Beads diagnostic uses the target home with a sanitized PATH"
fi

# Missing bd is advisory because Beads integration is optional.
find_beads_cli() {
    return 1
}
beads_output="$(diagnose_beads_cli 2>&1)"
beads_status=$?
if [ "$beads_status" -ne 0 ]; then
    echo "FAIL: Missing bd made the diagnostic fatal"
    failures=$((failures + 1))
elif ! grep -Fq "The Beads board stays hidden until bd is installed." <<< "$beads_output"; then
    echo "FAIL: Missing bd warning did not explain the hidden board"
    failures=$((failures + 1))
else
    echo "PASS: Missing bd emits a nonfatal Beads board warning"
fi
rm -rf "$beads_fixture"

if [ "$failures" -gt 0 ]; then
    echo "${failures} postinst regression test(s) failed."
    exit 1
fi
echo "All postinst regression tests passed."
exit 0
