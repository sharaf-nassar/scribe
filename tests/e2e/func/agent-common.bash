# Shared config-restart, in-pane CLI, and failure helpers for the functional
# agent E2E scripts (agent-read.sh, agent-world.sh, agent-write.sh). Each
# script still picks its own [agent_api] policy and its own CLI subcommand;
# only the mechanics of applying them are shared.

CONFIG_FILE="$HOME/.config/scribe/config.toml"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# Restarts the server and daemon against a fresh config.toml holding the given
# TOML body (typically one [agent_api] table).
restart_with_agent_config() {
    scribe-test daemon stop >/dev/null 2>&1 || true
    scribe-test server stop >/dev/null 2>&1 || true
    printf '%s\n' "$1" >"$CONFIG_FILE"
    scribe-test server start
    scribe-test daemon start
}

# Sends `scribe agent --agent <agent_name> <command>` inside a live pane,
# capturing stdout to $output (and stderr to its .stderr sibling), then
# printing "<label>:<exit status>" for a caller to `wait-output` on.
send_agent_cli() {
    local pane="$1" agent_name="$2" command="$3" output="$4" label="$5"
    scribe-test send "$pane" "RUST_LOG=off scribe agent --agent $agent_name $command > '$output' 2> '${output%.json}.stderr'; status=\$?; printf '$label:%s\\n' \"\$status\"\n"
}
