#!/bin/bash
set -euo pipefail
#
# Scribe — Auggie AI indicator hook setup
#
# Wires Augment Auggie's hook system to call `ai-hook-auggie.sh` for every
# state / Stop / SessionEnd event, replacing the legacy `auggie-state.sh`
# OSC-emitter that broke when AI tool hooks lost terminal access.
# Routes through the structured hook channel; see specs/003-ai-hook-channel/.
#
# Idempotent: safe to run multiple times.
#
# Usage:
#   setup-auggie-hooks.sh

AUGMENT_DIR="${HOME}/.augment"
SETTINGS="${AUGMENT_DIR}/settings.json"

# ── Step 1: Check that Augment/Auggie is installed ───────────────────────
if [[ ! -d "$AUGMENT_DIR" ]]; then
    echo "Augment directory (~/.augment) not found. Skipping hook setup."
    echo "Install or run Auggie first, then re-run: setup-auggie-hooks.sh"
    exit 0
fi

# ── Step 2: Merge Scribe hooks into settings.json ────────────────────────
python3 << 'PYEOF'
import json
import os
import sys

settings_path = os.path.expanduser("~/.augment/settings.json")


def find_scribe_install_prefix():
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
        if os.path.isfile(os.path.join(p, "ai-hook-auggie.sh")):
            return p
    return "/usr/share/scribe"


install_prefix = find_scribe_install_prefix()
adapter = os.path.join(install_prefix, "ai-hook-auggie.sh")

# (event, matcher_or_None, list-of-hook-dicts, metadata_or_None)
SCRIBE_HOOKS = [
    ("SessionStart", None, [
        {"type": "command", "command": f'{adapter} session_start'},
    ], None),
    ("PreToolUse", ".*", [
        {"type": "command", "command": f'{adapter} processing'},
    ], None),
    ("PostToolUse", ".*", [
        {"type": "command", "command": f'{adapter} processing'},
    ], None),
    ("Stop", None, [
        {"type": "command", "command": f'{adapter} stop'},
    ], {"includeConversationData": True}),
    ("SessionEnd", None, [
        {"type": "command", "command": f'{adapter} session_end'},
    ], None),
]


def is_scribe_hook(entry):
    for hook in entry.get("hooks", []):
        cmd = hook.get("command", "")
        # New marker:
        if "ai-hook-auggie.sh" in cmd:
            return True
        # Legacy markers (pre-AI-Hook-Channel install):
        if "AuggieState=" in cmd or "AuggieTaskLabel" in cmd or "auggie-state.sh" in cmd:
            return True
    return False


def merge_event_hooks(existing_entries, scribe_entries):
    kept = [entry for entry in existing_entries if not is_scribe_hook(entry)]
    return scribe_entries + kept


def strip_json5_comments_and_trailing_commas(text):
    without_comments = []
    in_string = False
    quote = ""
    escape = False
    in_line_comment = False
    in_block_comment = False
    i = 0
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""

        if in_line_comment:
            if ch in "\r\n":
                in_line_comment = False
                without_comments.append(ch)
            i += 1
            continue

        if in_block_comment:
            if ch == "*" and nxt == "/":
                in_block_comment = False
                i += 2
                continue
            if ch in "\r\n":
                without_comments.append(ch)
            i += 1
            continue

        if in_string:
            without_comments.append(ch)
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == quote:
                in_string = False
            i += 1
            continue

        if ch in ("\"", "'"):
            in_string = True
            quote = ch
            without_comments.append(ch)
            i += 1
            continue

        if ch == "/" and nxt == "/":
            in_line_comment = True
            i += 2
            continue

        if ch == "/" and nxt == "*":
            in_block_comment = True
            i += 2
            continue

        without_comments.append(ch)
        i += 1

    text = "".join(without_comments)
    without_trailing_commas = []
    in_string = False
    quote = ""
    escape = False
    i = 0
    while i < len(text):
        ch = text[i]
        if in_string:
            without_trailing_commas.append(ch)
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == quote:
                in_string = False
            i += 1
            continue

        if ch in ("\"", "'"):
            in_string = True
            quote = ch
            without_trailing_commas.append(ch)
            i += 1
            continue

        if ch == ",":
            j = i + 1
            while j < len(text) and text[j].isspace():
                j += 1
            if j < len(text) and text[j] in "}]":
                i += 1
                continue

        without_trailing_commas.append(ch)
        i += 1

    return "".join(without_trailing_commas)


def load_settings(path):
    with open(path) as f:
        content = f.read()
    try:
        return json.loads(content)
    except json.JSONDecodeError as json_error:
        try:
            return json.loads(strip_json5_comments_and_trailing_commas(content))
        except json.JSONDecodeError:
            raise json_error


if os.path.isfile(settings_path):
    try:
        settings = load_settings(settings_path)
    except json.JSONDecodeError as exc:
        print(f"ERROR: Cannot parse {settings_path}: {exc}", file=sys.stderr)
        print("Refusing to overwrite existing Augment settings.", file=sys.stderr)
        sys.exit(1)
else:
    settings = {}

if not isinstance(settings, dict):
    print(f"ERROR: {settings_path} must contain a JSON object.", file=sys.stderr)
    sys.exit(1)

hooks = settings.setdefault("hooks", {})
if not isinstance(hooks, dict):
    print(f"ERROR: {settings_path} field 'hooks' must be a JSON object.", file=sys.stderr)
    sys.exit(1)

scribe_by_event = {}
for event, matcher, hook_cmds, metadata in SCRIBE_HOOKS:
    entry = {"hooks": hook_cmds}
    if matcher is not None:
        entry["matcher"] = matcher
    if metadata is not None:
        entry["metadata"] = metadata
    scribe_by_event.setdefault(event, []).append(entry)

for event, scribe_entries in scribe_by_event.items():
    existing = hooks.get(event, [])
    if not isinstance(existing, list):
        print(f"ERROR: {settings_path} hooks.{event} must be an array.", file=sys.stderr)
        sys.exit(1)
    hooks[event] = merge_event_hooks(existing, scribe_entries)

settings["hooks"] = hooks

tmp_path = settings_path + ".tmp"
with open(tmp_path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
os.replace(tmp_path, settings_path)

print(f"  Updated {settings_path}")
print("  Scribe Auggie hooks routed via scribe-hook-helper IPC.")
PYEOF

echo ""
echo "  Done! Restart Auggie sessions for hooks to take effect."
