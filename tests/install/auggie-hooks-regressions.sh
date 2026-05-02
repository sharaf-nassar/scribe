#!/bin/bash
# Offline regression tests for Scribe's Auggie hook setup helper.

set -u

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
setup_script="${repo_root}/dist/setup-auggie-hooks.sh"

if [ ! -x "$setup_script" ]; then
    echo "FAIL: setup-auggie-hooks.sh not found or not executable at ${setup_script}" >&2
    exit 2
fi

failures=0
tmp_home="$(mktemp -d)"
log_file="${tmp_home}/setup.log"
trap 'rm -rf "$tmp_home"' EXIT

mkdir -p "${tmp_home}/.augment"
cat > "${tmp_home}/.augment/settings.json" <<'JSON5'
{
  // Auggie accepts JSON5 settings, including comments.
  "theme": "default-dark",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "view",
        "hooks": [
          {
            "type": "command",
            "command": "/usr/local/bin/user-hook.sh",
          },
        ],
      },
    ],
  },
}
JSON5

if HOME="$tmp_home" bash "$setup_script" --hook-source "${repo_root}/dist" >"$log_file" 2>&1; then
    python3 - "$tmp_home" <<'PY'
import json
import os
import sys

home = sys.argv[1]
settings_path = os.path.join(home, ".augment", "settings.json")
with open(settings_path) as handle:
    settings = json.load(handle)

hooks = settings.get("hooks", {})
assert settings.get("theme") == "default-dark"
assert hooks["PreToolUse"][0]["hooks"][0]["command"].endswith("/.augment/hooks/auggie-state.sh")
assert hooks["PreToolUse"][1]["hooks"][0]["command"] == "/usr/local/bin/user-hook.sh"
assert hooks["Stop"][0]["metadata"]["includeConversationData"] is True
PY
    if [ "$?" -eq 0 ]; then
        echo "PASS: setup-auggie-hooks accepts JSON5 settings and preserves user hooks"
    else
        echo "FAIL: merged Auggie settings did not contain expected hooks"
        failures=$((failures + 1))
    fi
else
    echo "FAIL: setup-auggie-hooks rejected JSON5 settings"
    cat "$log_file"
    failures=$((failures + 1))
fi

if [ "$failures" -gt 0 ]; then
    echo "${failures} Auggie hook regression test(s) failed."
    exit 1
fi

echo "All Auggie hook regression tests passed."
