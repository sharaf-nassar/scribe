#!/bin/bash
set -euo pipefail
#
# Scribe — Codex Code AI indicator hook setup
#
# Installs a Stop-hook helper and configures Codex hooks so Scribe receives
# provider-aware OSC 1337 state updates from Codex CLI.
#
# Idempotent: safe to run multiple times. Only adds/updates Scribe-managed
# Codex hook entries and preserves unrelated hooks.
#
# Usage:
#   setup-codex-hooks.sh [--hook-source DIR]
#
#   --hook-source DIR   Directory containing detect-codex-question.sh.
#                       Defaults to the same directory as this script.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HOOK_SOURCE="${SCRIPT_DIR}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --hook-source) HOOK_SOURCE="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

CODEX_DIR="${HOME}/.codex"
HOOKS_DIR="${CODEX_DIR}/hooks"
CONFIG_TOML="${CODEX_DIR}/config.toml"
HOOKS_JSON="${CODEX_DIR}/hooks.json"
HOOK_SCRIPTS=(
    "detect-codex-question.sh"
    "codex-task-label.sh"
)

# ── Step 1: Check that Codex is installed ────────────────────────────────
if [[ ! -d "$CODEX_DIR" ]]; then
    echo "Codex directory (~/.codex) not found. Skipping hook setup."
    echo "Install or run Codex first, then re-run: setup-codex-hooks.sh"
    exit 0
fi

# ── Step 2: Install the hook helper ──────────────────────────────────────
mkdir -p "$HOOKS_DIR"

for hook_script in "${HOOK_SCRIPTS[@]}"; do
    SRC="${HOOK_SOURCE}/${hook_script}"
    DEST="${HOOKS_DIR}/${hook_script}"

    if [[ ! -f "$SRC" ]]; then
        echo "ERROR: Hook source not found: ${SRC}" >&2
        exit 1
    fi

    cp "$SRC" "$DEST"
    chmod +x "$DEST"
    echo "  Installed ${DEST}"
done

# ── Step 3: Enable Codex hooks in config.toml ────────────────────────────
python3 << 'PYEOF'
import os
from pathlib import Path

config_path = Path(os.path.expanduser("~/.codex/config.toml"))
text = config_path.read_text() if config_path.exists() else ""

lines = text.splitlines()

features_start = None
features_end = len(lines)
for idx, line in enumerate(lines):
    if line.strip() == "[features]":
        features_start = idx
        for next_idx in range(idx + 1, len(lines)):
            if lines[next_idx].startswith("[") and lines[next_idx].endswith("]"):
                features_end = next_idx
                break
        break

if features_start is None:
    if text and not text.endswith("\n"):
        text += "\n"
    if text:
        text += "\n"
    text += "[features]\n"
    text += "codex_hooks = true\n"
else:
    replaced = False
    for idx in range(features_start + 1, features_end):
        if lines[idx].split("=", 1)[0].strip() == "codex_hooks":
            lines[idx] = "codex_hooks = true"
            replaced = True
            break
    if not replaced:
        lines.insert(features_end, "codex_hooks = true")
    text = "\n".join(lines)
    if lines:
        text += "\n"

config_path.write_text(text)
print(f"  Updated {config_path}")
print("  Enabled [features].codex_hooks = true")
PYEOF

# ── Step 4: Merge Scribe hooks into hooks.json ───────────────────────────
python3 << 'PYEOF'
import json
import os

hooks_path = os.path.expanduser("~/.codex/hooks.json")
hooks_dir = os.path.expanduser("~/.codex/hooks")
stop_hook_script = os.path.join(hooks_dir, "detect-codex-question.sh")
task_label_script = os.path.join(hooks_dir, "codex-task-label.sh")

SCRIBE_HOOKS = [
    ("SessionStart", "startup|resume", [
        {"type": "command", "command": f'"{task_label_script}" session-start'},
    ]),
    ("UserPromptSubmit", None, [
        {"type": "command", "command": f'"{task_label_script}" user-prompt-submit'},
    ]),
    ("PreToolUse", "Bash", [
        {"type": "command", "command": f'"{task_label_script}" tool-processing'},
    ]),
    ("PostToolUse", "Bash", [
        {"type": "command", "command": f'"{task_label_script}" tool-processing'},
    ]),
]

def is_scribe_hook(entry):
    for hook in entry.get("hooks", []):
        cmd = hook.get("command", "")
        if "CodexState=" in cmd or "CodexTaskLabel" in cmd or "detect-codex-question" in cmd or "codex-task-label" in cmd:
            return True
    return False

def merge_event_hooks(existing_entries, scribe_entries):
    kept = [entry for entry in existing_entries if not is_scribe_hook(entry)]
    return scribe_entries + kept

if os.path.isfile(hooks_path):
    with open(hooks_path) as f:
        config = json.load(f)
else:
    config = {}

hooks = config.setdefault("hooks", {})

scribe_by_event = {}
for event, matcher, hook_cmds in SCRIBE_HOOKS:
    entry = {"hooks": hook_cmds}
    if matcher is not None:
        entry["matcher"] = matcher
    scribe_by_event.setdefault(event, []).append(entry)

stop_entry = {
    "hooks": [{"type": "command", "command": stop_hook_script, "timeout": 30}],
}
scribe_by_event.setdefault("Stop", []).append(stop_entry)

for event, scribe_entries in scribe_by_event.items():
    existing = hooks.get(event, [])
    hooks[event] = merge_event_hooks(existing, scribe_entries)

config["hooks"] = hooks

tmp_path = hooks_path + ".tmp"
with open(tmp_path, "w") as f:
    json.dump(config, f, indent=2)
    f.write("\n")
os.replace(tmp_path, hooks_path)

print(f"  Updated {hooks_path}")
print("  Scribe Codex hooks are configured.")
PYEOF

echo ""
echo "  Done! Restart Codex sessions for hooks to take effect."
