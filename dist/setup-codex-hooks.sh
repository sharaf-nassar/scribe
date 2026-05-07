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
    "detect-codex-context.sh"
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
    text += "hooks = true\n"
else:
    replaced = False
    next_lines = lines[:features_start + 1]
    for line in lines[features_start + 1:features_end]:
        key = line.split("=", 1)[0].strip()
        if key == "hooks":
            next_lines.append("hooks = true")
            replaced = True
        elif key != "codex_hooks":
            next_lines.append(line)
    if not replaced:
        next_lines.append("hooks = true")
    lines = next_lines + lines[features_end:]
    text = "\n".join(lines)
    if lines:
        text += "\n"

config_path.write_text(text)
print(f"  Updated {config_path}")
print("  Enabled [features].hooks = true")
PYEOF

# ── Step 4: Merge Scribe hooks into hooks.json ───────────────────────────
python3 << 'PYEOF'
import json
import os
import re

hooks_path = os.path.expanduser("~/.codex/hooks.json")
hooks_dir = os.path.expanduser("~/.codex/hooks")
config_path = os.path.expanduser("~/.codex/config.toml")
stop_hook_script = os.path.join(hooks_dir, "detect-codex-question.sh")
task_label_script = os.path.join(hooks_dir, "codex-task-label.sh")
context_hook_script = os.path.join(hooks_dir, "detect-codex-context.sh")
HOOK_EVENTS = (
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
)
SCRIBE_MARKERS = (
    "CodexState=",
    "CodexTaskLabel",
    "detect-codex-question",
    "codex-task-label",
    "detect-codex-context",
)

SCRIBE_HOOKS = [
    ("SessionStart", "startup|resume", [
        {"type": "command", "command": f'"{task_label_script}" session-start'},
    ]),
    ("UserPromptSubmit", None, [
        {"type": "command", "command": f'"{task_label_script}" user-prompt-submit'},
        {"type": "command", "command": "python3 -c 'import json,sys;d=json.load(sys.stdin);sid=d.get(\"session_id\",\"\");p=d.get(\"prompt\",\"\")[:256].replace(chr(7),\"\").replace(chr(27),\"\");f=open(\"/dev/tty\",\"w\");f.write(f\"\\x1b]1337;CodexState=processing;conversation_id={sid}\\x07\" if sid else \"\\x1b]1337;CodexState=processing\\x07\");f.write(f\"\\x1b]1337;CodexPrompt={p}\\x07\") if p else None;f.close()' 2>/dev/null || true"},
    ]),
    ("PreToolUse", "Bash", [
        {"type": "command", "command": f'"{task_label_script}" tool-processing'},
    ]),
    ("PostToolUse", "Bash", [
        {"type": "command", "command": f'"{task_label_script}" tool-processing'},
    ]),
]

def command_is_scribe(cmd):
    return any(marker in cmd for marker in SCRIBE_MARKERS)

def is_scribe_hook(entry):
    for hook in entry.get("hooks", []):
        cmd = hook.get("command", "")
        if command_is_scribe(cmd):
            return True
    return False

def merge_event_hooks(existing_entries, scribe_entries):
    kept = [entry for entry in existing_entries if not is_scribe_hook(entry)]
    return scribe_entries + kept

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

# Context % producer: runs on every PostToolUse (no matcher) and Stop.
context_post_tool_entry = {
    "hooks": [{"type": "command", "command": context_hook_script, "timeout": 10}],
}
scribe_by_event.setdefault("PostToolUse", []).append(context_post_tool_entry)

context_stop_entry = {
    "hooks": [{"type": "command", "command": context_hook_script, "timeout": 10}],
}
scribe_by_event.setdefault("Stop", []).append(context_stop_entry)

def read_hooks_json():
    if not os.path.isfile(hooks_path):
        return {}
    with open(hooks_path) as f:
        return json.load(f)

def write_hooks_json(config):
    tmp_path = hooks_path + ".tmp"
    with open(tmp_path, "w") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
    os.replace(tmp_path, hooks_path)

def inline_hooks_present(text):
    events = "|".join(re.escape(event) for event in HOOK_EVENTS)
    pattern = re.compile(rf"^\s*\[\[hooks\.({events})(?:\.hooks)?\]\]\s*$", re.MULTILINE)
    return bool(pattern.search(text))

def hook_group_header(line):
    stripped = line.strip()
    for event in HOOK_EVENTS:
        if stripped == f"[[hooks.{event}]]":
            return event
    return None

def hook_handler_header(line, event):
    return line.strip() == f"[[hooks.{event}.hooks]]"

def any_section_header(line):
    stripped = line.strip()
    return stripped.startswith("[") and stripped.endswith("]")

def strip_scribe_inline_hooks(text):
    lines = text.splitlines()
    output = []
    idx = 0
    while idx < len(lines):
        event = hook_group_header(lines[idx])
        if event is None:
            output.append(lines[idx])
            idx += 1
            continue

        group = [lines[idx]]
        idx += 1
        while idx < len(lines):
            if hook_group_header(lines[idx]) is not None:
                break
            if any_section_header(lines[idx]) and not hook_handler_header(lines[idx], event):
                break
            group.append(lines[idx])
            idx += 1

        if not any(command_is_scribe(line) for line in group):
            output.extend(group)
            continue

        prefix = []
        handler_blocks = []
        cursor = 0
        while cursor < len(group):
            if hook_handler_header(group[cursor], event):
                block = [group[cursor]]
                cursor += 1
                while cursor < len(group) and not hook_handler_header(group[cursor], event):
                    block.append(group[cursor])
                    cursor += 1
                handler_blocks.append(block)
            else:
                prefix.append(group[cursor])
                cursor += 1

        kept_blocks = [
            block for block in handler_blocks if not any(command_is_scribe(line) for line in block)
        ]
        if kept_blocks:
            output.extend(prefix)
            for block in kept_blocks:
                output.extend(block)

    return "\n".join(output).rstrip() + "\n"

def toml_value(value):
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{key} = {toml_value(val)}" for key, val in value.items()) + " }"
    if value is None:
        return '""'
    return json.dumps(str(value))

def render_inline_entry(event, entry):
    lines = [f"[[hooks.{event}]]"]
    if entry.get("matcher") is not None:
        lines.append(f"matcher = {toml_value(entry['matcher'])}")
    for hook in entry.get("hooks", []):
        lines.append("")
        lines.append(f"[[hooks.{event}.hooks]]")
        for key in ("type", "command", "timeout", "statusMessage", "async"):
            if key in hook:
                lines.append(f"{key} = {toml_value(hook[key])}")
        for key, value in hook.items():
            if key not in {"type", "command", "timeout", "statusMessage", "async"}:
                lines.append(f"{key} = {toml_value(value)}")
    return "\n".join(lines)

def append_inline_entries(text, entries_by_event):
    chunks = []
    for event in HOOK_EVENTS:
        for entry in entries_by_event.get(event, []):
            chunks.append(render_inline_entry(event, entry))
    if not chunks:
        return text
    return text.rstrip() + "\n\n" + "\n\n".join(chunks) + "\n"

config_text = open(config_path).read() if os.path.isfile(config_path) else ""
if inline_hooks_present(config_text):
    hooks_json_config = read_hooks_json()
    migrated_by_event = {}
    for event, entries in hooks_json_config.get("hooks", {}).items():
        for entry in entries if isinstance(entries, list) else []:
            if not is_scribe_hook(entry):
                migrated_by_event.setdefault(event, []).append(entry)

    inline_by_event = {}
    for event in HOOK_EVENTS:
        inline_by_event[event] = migrated_by_event.get(event, []) + scribe_by_event.get(event, [])

    config_text = strip_scribe_inline_hooks(config_text)
    config_text = append_inline_entries(config_text, inline_by_event)
    tmp_path = config_path + ".tmp"
    with open(tmp_path, "w") as f:
        f.write(config_text)
    os.replace(tmp_path, config_path)

    if os.path.exists(hooks_path):
        os.remove(hooks_path)
        print(f"  Removed {hooks_path} after migrating hooks into config.toml")
    print(f"  Updated {config_path}")
    print("  Scribe Codex hooks are configured inline.")
else:
    config = read_hooks_json()
    hooks = config.setdefault("hooks", {})

    for event, scribe_entries in scribe_by_event.items():
        existing = hooks.get(event, [])
        hooks[event] = merge_event_hooks(existing, scribe_entries)

    config["hooks"] = hooks
    write_hooks_json(config)
    print(f"  Updated {hooks_path}")
    print("  Scribe Codex hooks are configured.")
PYEOF

echo ""
echo "  Done! Restart Codex sessions for hooks to take effect."
