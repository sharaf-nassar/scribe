#!/bin/bash
set -euo pipefail
#
# Scribe — Claude Code AI indicator hook setup
#
# Installs the question-detection hook script and configures Claude Code
# settings.json so that Scribe's AI state indicators work out of the box.
#
# Idempotent: safe to run multiple times. Only adds/updates Scribe-specific
# hooks; existing hooks (quill, plugins, etc.) are preserved.
#
# Usage:
#   setup-claude-hooks.sh [--hook-source DIR]
#
#   --hook-source DIR   Directory containing detect-claude-question.sh.
#                       Defaults to the same directory as this script.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HOOK_SOURCE="${SCRIPT_DIR}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --hook-source) HOOK_SOURCE="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

CLAUDE_DIR="${HOME}/.claude"
HOOKS_DIR="${CLAUDE_DIR}/hooks"
SETTINGS="${CLAUDE_DIR}/settings.json"
HOOK_SCRIPT="detect-claude-question.sh"

# ── Step 1: Check that Claude Code is installed ─────────────────────────
if [[ ! -d "$CLAUDE_DIR" ]]; then
    echo "Claude Code directory (~/.claude) not found. Skipping hook setup."
    echo "Install Claude Code first, then re-run: setup-claude-hooks.sh"
    exit 0
fi

# ── Step 2: Install the hook script ─────────────────────────────────────
mkdir -p "$HOOKS_DIR"

SRC="${HOOK_SOURCE}/${HOOK_SCRIPT}"
DEST="${HOOKS_DIR}/${HOOK_SCRIPT}"

if [[ ! -f "$SRC" ]]; then
    echo "ERROR: Hook source not found: ${SRC}" >&2
    exit 1
fi

cp "$SRC" "$DEST"
chmod +x "$DEST"
echo "  Installed ${DEST}"

# ── Step 3: Merge Scribe hooks into settings.json ──────────────────────
# Uses Python (available on virtually all systems) for safe JSON manipulation.
# The merge logic:
#   - For each hook event (Stop, Notification, etc.), find existing Scribe
#     entries by matching the command path prefix and replace them.
#   - Non-Scribe entries (quill, plugins) are left untouched.
#   - If settings.json doesn't exist, create it with just the hooks.

python3 << 'PYEOF'
import json
import os
import sys

settings_path = os.path.expanduser("~/.claude/settings.json")
hooks_dir = os.path.expanduser("~/.claude/hooks")
hook_script = os.path.join(hooks_dir, "detect-claude-question.sh")

# The Scribe-specific hooks to install.
# Each entry: (event, matcher_or_none, hook_commands)
# matcher_or_none=None means no matcher field (matches everything).
SCRIBE_HOOKS = [
    ("Notification", "idle_prompt", [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=idle_prompt\\a' > /dev/tty"},
    ]),
    ("Notification", "permission_prompt", [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=permission_prompt\\a' > /dev/tty"},
    ]),
    ("Notification", "error", [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=error\\a' > /dev/tty"},
    ]),
    ("PreToolUse", "AskUserQuestion", [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=waiting_for_input\\a' > /dev/tty"},
    ]),
    ("PostToolUse", "AskUserQuestion", [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=processing\\a' > /dev/tty"},
    ]),
    ("UserPromptSubmit", None, [
        {"type": "command", "command": "printf '\\e]1337;ClaudeState=processing\\a' > /dev/tty"},
    ]),
]

# The Stop hook is special: it uses the detection script instead of a
# hardcoded printf, so we identify it by the script path.
STOP_HOOK_CMD = hook_script


def is_scribe_hook(entry):
    """Return True if a hook entry was installed by Scribe."""
    for h in entry.get("hooks", []):
        cmd = h.get("command", "")
        if "ClaudeState=" in cmd or "detect-claude-question" in cmd:
            return True
    return False


def merge_event_hooks(existing_entries, scribe_entries):
    """Merge Scribe hooks into existing entries for a single event type.

    Removes old Scribe entries and prepends the new ones. Non-Scribe
    entries are preserved in their original order.
    """
    # Keep non-Scribe entries
    kept = [e for e in existing_entries if not is_scribe_hook(e)]
    return scribe_entries + kept


# Load or create settings
if os.path.isfile(settings_path):
    with open(settings_path) as f:
        settings = json.load(f)
else:
    settings = {}

hooks = settings.setdefault("hooks", {})

# ── Build Scribe entries per event type ──────────────────────────────
scribe_by_event: dict[str, list] = {}

for event, matcher, hook_cmds in SCRIBE_HOOKS:
    entry: dict = {"hooks": hook_cmds}
    if matcher is not None:
        entry["matcher"] = matcher
    scribe_by_event.setdefault(event, []).append(entry)

# Add the Stop hook with the detection script
stop_entry = {
    "matcher": "",
    "hooks": [{"type": "command", "command": hook_script}],
}
scribe_by_event.setdefault("Stop", []).append(stop_entry)

# ── Merge into settings ─────────────────────────────────────────────
for event, scribe_entries in scribe_by_event.items():
    existing = hooks.get(event, [])
    hooks[event] = merge_event_hooks(existing, scribe_entries)

settings["hooks"] = hooks

# Write atomically via tmp + rename
tmp_path = settings_path + ".tmp"
with open(tmp_path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
os.replace(tmp_path, settings_path)

print(f"  Updated {settings_path}")
print("  Scribe Claude Code hooks are configured.")
PYEOF

echo ""
echo "  Done! Restart Claude Code for hooks to take effect."
