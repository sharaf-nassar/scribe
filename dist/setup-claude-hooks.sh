#!/bin/bash
set -euo pipefail
#
# Scribe — Claude Code AI indicator hook setup
#
# Wires Claude Code's hook system to call `ai-hook-claude.sh` for every
# state/prompt/Stop event, and registers `ai-hook-statusline.sh` as the
# statusLine subprocess. Replaces the OSC-over-/dev/tty hooks that broke
# in Claude Code v2.1.139 (intentional TTY detachment from hook subprocs).
#
# All Scribe hook commands resolve at runtime to:
#   <install_prefix>/ai-hook-claude.sh <event-name>
# where <install_prefix> is /usr/share/scribe, /usr/share/scribe-dev, the
# macOS bundle Resources/dist directory, or whatever
# SCRIBE_INSTALL_PREFIX points to.
#
# Idempotent: safe to run multiple times. Removes Scribe-owned hooks
# installed by previous versions before rewriting. Non-Scribe hooks
# (quill, plugins, the user's own) are preserved untouched.
#
# Usage:
#   setup-claude-hooks.sh

CLAUDE_DIR="${HOME}/.claude"
SETTINGS="${CLAUDE_DIR}/settings.json"

# ── Step 1: Check that Claude Code is installed ─────────────────────────
if [[ ! -d "$CLAUDE_DIR" ]]; then
    echo "Claude Code directory (~/.claude) not found. Skipping hook setup."
    echo "Install Claude Code first, then re-run: setup-claude-hooks.sh"
    exit 0
fi

# ── Step 2: Merge Scribe hooks into settings.json ──────────────────────
# All hook commands point at ai-hook-claude.sh in the install prefix.
# Removes legacy `printf > /dev/tty` Scribe hooks and the obsolete
# detect-claude-question.sh Stop hook before inserting the new entries.

python3 << 'PYEOF'
import json
import os
import sys

settings_path = os.path.expanduser("~/.claude/settings.json")


def find_scribe_install_prefix():
    """Locate the directory containing ai-hook-claude.sh and ai-hook-statusline.sh."""
    env = os.environ.get("SCRIBE_INSTALL_PREFIX")
    if env and os.path.isdir(env):
        return env
    for p in (
        "/usr/share/scribe",
        "/usr/share/scribe-dev",
        "/usr/local/share/scribe",
        "/usr/local/share/scribe-dev",
        "/Applications/Scribe.app/Contents/Resources/dist",
        "/Applications/Scribe-Dev.app/Contents/Resources/dist",
    ):
        if os.path.isfile(os.path.join(p, "ai-hook-claude.sh")):
            return p
    return "/usr/share/scribe"


install_prefix = find_scribe_install_prefix()
adapter = os.path.join(install_prefix, "ai-hook-claude.sh")
statusline = os.path.join(install_prefix, "ai-hook-statusline.sh")

# Each tuple: (event, matcher_or_None, list-of-hook-command-dicts).
SCRIBE_HOOKS = [
    ("Notification", "permission_prompt", [
        {"type": "command", "command": f"{adapter} permission_prompt"},
    ]),
    ("Notification", "error", [
        {"type": "command", "command": f"{adapter} error"},
    ]),
    ("PreToolUse", "AskUserQuestion", [
        {"type": "command", "command": f"{adapter} pre_ask_user_question"},
    ]),
    ("PostToolUse", "AskUserQuestion", [
        {"type": "command", "command": f"{adapter} post_ask_user_question"},
    ]),
    ("UserPromptSubmit", None, [
        {"type": "command", "command": f"{adapter} user_prompt_submit"},
    ]),
    ("Stop", None, [
        {"type": "command", "command": f"{adapter} stop"},
    ]),
]


def is_scribe_hook(entry):
    """Return True if a hook entry was installed by Scribe (any version)."""
    for h in entry.get("hooks", []):
        cmd = h.get("command", "")
        # Legacy markers (pre-AI-Hook-Channel installs):
        if "ClaudeState=" in cmd or "detect-claude-question" in cmd:
            return True
        # Current marker: any path ending in ai-hook-claude.sh.
        if "ai-hook-claude.sh" in cmd:
            return True
    return False


def merge_event_hooks(existing_entries, scribe_entries):
    """Drop any Scribe-owned entries, prepend the new ones."""
    kept = [e for e in existing_entries if not is_scribe_hook(e)]
    return scribe_entries + kept


if os.path.isfile(settings_path):
    with open(settings_path) as f:
        settings = json.load(f)
else:
    settings = {}

hooks = settings.setdefault("hooks", {})

scribe_by_event: dict[str, list] = {}
for event, matcher, hook_cmds in SCRIBE_HOOKS:
    entry: dict = {"hooks": hook_cmds}
    if matcher is not None:
        entry["matcher"] = matcher
    scribe_by_event.setdefault(event, []).append(entry)

for event, scribe_entries in scribe_by_event.items():
    existing = hooks.get(event, [])
    hooks[event] = merge_event_hooks(existing, scribe_entries)

settings["hooks"] = hooks

# ── statusLine ───────────────────────────────────────────────────────
# Remove any Scribe-owned statusLine before re-inserting so a path move
# (e.g. switching from scribe-claude-statusline.sh to ai-hook-statusline.sh)
# updates cleanly.
existing_sl = settings.get("statusLine")
if isinstance(existing_sl, dict):
    cmd = existing_sl.get("command", "")
    if cmd.endswith("scribe-claude-statusline.sh") or cmd.endswith("ai-hook-statusline.sh"):
        settings.pop("statusLine", None)

existing_sl = settings.get("statusLine")
if existing_sl is None:
    settings["statusLine"] = {
        "type": "command",
        "command": statusline,
        "padding": 0,
    }
elif isinstance(existing_sl, dict) and existing_sl.get("command", "").endswith(
    ("scribe-claude-statusline.sh", "ai-hook-statusline.sh")
):
    existing_sl["command"] = statusline
    existing_sl.setdefault("type", "command")
    existing_sl.setdefault("padding", 0)
else:
    print(
        "scribe: leaving custom statusLine intact; "
        "Claude context % will not be displayed unless you point statusLine at "
        + statusline,
        file=sys.stderr,
    )

# Atomic write via tmp + rename.
tmp_path = settings_path + ".tmp"
with open(tmp_path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
os.replace(tmp_path, settings_path)

print(f"  Updated {settings_path}")
print("  Scribe Claude Code hooks now route via scribe-hook-helper IPC.")
PYEOF

echo ""
echo "  Done! Restart Claude Code for hooks to take effect."
