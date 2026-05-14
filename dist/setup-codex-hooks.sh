#!/bin/bash
set -euo pipefail
#
# Scribe — Codex Code AI indicator hook setup
#
# Wires Codex's hook system to call `ai-hook-codex.sh` for every state /
# prompt / Stop / context event, replacing the legacy multi-script
# legacy tty-writing install that broke when AI tool hooks lost terminal
# access. Routes through the structured hook channel; see
# specs/003-ai-hook-channel/.
#
# Idempotent: safe to run multiple times. Removes Scribe-owned hook entries
# installed by previous versions before rewriting. Non-Scribe entries are
# preserved.
#
# Usage:
#   setup-codex-hooks.sh

HOOK_SOURCE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --hook-source)
            HOOK_SOURCE="${2:-}"
            shift 2
            ;;
        --hook-source=*)
            HOOK_SOURCE="${1#--hook-source=}"
            shift
            ;;
        *)
            shift
            ;;
    esac
done

if [[ -n "$HOOK_SOURCE" ]]; then
    export SCRIBE_INSTALL_PREFIX="$HOOK_SOURCE"
fi

CODEX_DIR="${HOME}/.codex"
CONFIG_TOML="${CODEX_DIR}/config.toml"
HOOKS_JSON="${CODEX_DIR}/hooks.json"

# ── Step 1: Check that Codex is installed ────────────────────────────────
if [[ ! -d "$CODEX_DIR" ]]; then
    echo "Codex directory (~/.codex) not found. Skipping hook setup."
    echo "Install or run Codex first, then re-run: setup-codex-hooks.sh"
    exit 0
fi

# ── Step 2: Enable Codex hooks in config.toml ────────────────────────────
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
    text += "codex_hooks = true\n"
else:
    hooks_replaced = False
    codex_hooks_replaced = False
    next_lines = lines[:features_start + 1]
    for line in lines[features_start + 1:features_end]:
        key = line.split("=", 1)[0].strip()
        if key == "hooks":
            next_lines.append("hooks = true")
            hooks_replaced = True
        elif key == "codex_hooks":
            next_lines.append("codex_hooks = true")
            codex_hooks_replaced = True
        else:
            next_lines.append(line)
    if not hooks_replaced:
        next_lines.append("hooks = true")
    if not codex_hooks_replaced:
        next_lines.append("codex_hooks = true")
    lines = next_lines + lines[features_end:]
    text = "\n".join(lines)
    if lines:
        text += "\n"

config_path.write_text(text)
print(f"  Updated {config_path}")
print("  Enabled [features].hooks = true")
print("  Enabled [features].codex_hooks = true")
PYEOF

# ── Step 3: Merge Scribe hooks into hooks.json or inline TOML ────────────
python3 << 'PYEOF'
import hashlib
import json
import os
import re

hooks_path = os.path.expanduser("~/.codex/hooks.json")
config_path = os.path.expanduser("~/.codex/config.toml")


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
        if os.path.isfile(os.path.join(p, "ai-hook-codex.sh")):
            return p
    return "/usr/share/scribe"


install_prefix = find_scribe_install_prefix()
adapter = os.path.join(install_prefix, "ai-hook-codex.sh")

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
HOOK_EVENT_LABELS = {
    "PreToolUse": "pre_tool_use",
    "PermissionRequest": "permission_request",
    "PostToolUse": "post_tool_use",
    "PreCompact": "pre_compact",
    "PostCompact": "post_compact",
    "SessionStart": "session_start",
    "UserPromptSubmit": "user_prompt_submit",
    "Stop": "stop",
}
MATCHER_EVENTS = {
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
}
# Strings that identify a hook entry as Scribe-owned (any version).
# Includes legacy markers so old installs migrate cleanly.
SCRIBE_MARKERS = (
    "ai-hook-codex.sh",
    # Legacy (pre-AI-Hook-Channel install) markers:
    "Codex" "State=",
    "Codex" "Prompt=",
    "Codex" "TaskLabel",
    "codex-prompt-state",
    "detect-codex-question",
    "codex-task-label",
    "detect-codex-context",
    "codex-hook-common",
)

SCRIBE_HOOKS = [
    ("SessionStart", "startup|resume|clear", [
        {"type": "command", "command": f'"{adapter}" session_start'},
    ]),
    ("UserPromptSubmit", None, [
        {"type": "command", "command": f'"{adapter}" user_prompt_submit'},
    ]),
    ("PermissionRequest", None, [
        {"type": "command", "command": f'"{adapter}" permission_request'},
    ]),
    ("PreToolUse", None, [
        {"type": "command", "command": f'"{adapter}" tool_processing'},
    ]),
    ("PostToolUse", None, [
        {"type": "command", "command": f'"{adapter}" tool_processing'},
    ]),
    ("Stop", None, [
        {"type": "command", "command": f'"{adapter}" stop', "timeout": 30},
    ]),
    # Context % producer also fires on PostToolUse (no matcher) and Stop.
    ("PostToolUse", None, [
        {"type": "command", "command": f'"{adapter}" context', "timeout": 10},
    ]),
    ("Stop", None, [
        {"type": "command", "command": f'"{adapter}" context', "timeout": 10},
    ]),
]


def command_is_scribe(cmd):
    return any(marker in cmd for marker in SCRIBE_MARKERS)


def is_scribe_hook(entry):
    for hook in entry.get("hooks", []):
        if command_is_scribe(hook.get("command", "")):
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


def count_inline_groups(text):
    counts = {event: 0 for event in HOOK_EVENTS}
    for line in text.splitlines():
        event = hook_group_header(line)
        if event is not None:
            counts[event] += 1
    return counts


def parse_toml_scalar(value):
    trimmed = strip_toml_comment(value).strip()
    if trimmed.startswith('"'):
        return json.loads(trimmed)
    if trimmed.startswith("'"):
        end = trimmed.find("'", 1)
        return trimmed[1:end] if end != -1 else trimmed[1:]
    if trimmed == "true":
        return True
    if trimmed == "false":
        return False
    try:
        return int(trimmed)
    except ValueError:
        pass
    try:
        return float(trimmed)
    except ValueError:
        return trimmed


def strip_toml_comment(value):
    in_basic = False
    in_literal = False
    escaped = False
    for idx, char in enumerate(value):
        if in_basic:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_basic = False
            continue
        if in_literal:
            if char == "'":
                in_literal = False
            continue
        if char == '"':
            in_basic = True
        elif char == "'":
            in_literal = True
        elif char == "#":
            return value[:idx]
    return value


def parse_inline_hooks(text):
    hooks_by_event = {event: [] for event in HOOK_EVENTS}
    current_event = None
    current_entry = None
    current_hook = None

    for line in text.splitlines():
        event = hook_group_header(line)
        if event is not None:
            current_event = event
            current_entry = {"hooks": []}
            current_hook = None
            hooks_by_event[event].append(current_entry)
            continue

        if current_event is not None and hook_handler_header(line, current_event):
            current_hook = {}
            current_entry["hooks"].append(current_hook)
            continue

        if any_section_header(line):
            current_event = None
            current_entry = None
            current_hook = None
            continue

        if current_entry is None or "=" not in line:
            continue

        key, value = line.split("=", 1)
        target = current_hook if current_hook is not None else current_entry
        target[key.strip()] = parse_toml_scalar(value)

    return hooks_by_event


def normalized_command_hook(hook):
    normalized = {
        "async": hook.get("async", False),
        "command": hook["command"],
        "timeout": hook.get("timeout", 600),
        "type": "command",
    }
    if hook.get("statusMessage") is not None:
        normalized["statusMessage"] = hook["statusMessage"]
    return normalized


def command_hook_trusted_hash(event, entry, hook):
    identity = {
        "event_name": HOOK_EVENT_LABELS[event],
        "hooks": [normalized_command_hook(hook)],
    }
    if event in MATCHER_EVENTS and entry.get("matcher") is not None:
        identity["matcher"] = entry["matcher"]
    canonical = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return "sha256:" + hashlib.sha256(canonical).hexdigest()


def scribe_trust_entries_for(source_path, hooks_by_event, base_indices=None):
    base_indices = base_indices or {}
    trust_entries = []
    for event in HOOK_EVENTS:
        groups = hooks_by_event.get(event, [])
        if not isinstance(groups, list):
            continue
        for group_offset, entry in enumerate(groups):
            if not isinstance(entry, dict):
                continue
            group_index = base_indices.get(event, 0) + group_offset
            for hook_index, hook in enumerate(entry.get("hooks", [])):
                if not isinstance(hook, dict):
                    continue
                if not command_is_scribe(hook.get("command", "")):
                    continue
                key = f"{source_path}:{HOOK_EVENT_LABELS[event]}:{group_index}:{hook_index}"
                trust_entries.append((key, command_hook_trusted_hash(event, entry, hook)))
    return trust_entries


def prior_trust_entries_for(source_path, hooks_by_event, existing_state, base_indices=None):
    base_indices = base_indices or {}
    trusted_by_hash = {}
    for values in existing_state.values():
        trusted_hash = values.get("trusted_hash")
        if trusted_hash:
            trusted_by_hash.setdefault(trusted_hash, values)

    trust_entries = []
    for event in HOOK_EVENTS:
        groups = hooks_by_event.get(event, [])
        if not isinstance(groups, list):
            continue
        for group_offset, entry in enumerate(groups):
            if not isinstance(entry, dict):
                continue
            group_index = base_indices.get(event, 0) + group_offset
            for hook_index, hook in enumerate(entry.get("hooks", [])):
                if not isinstance(hook, dict) or not hook.get("command"):
                    continue
                if command_is_scribe(hook.get("command", "")):
                    continue
                trusted_hash = command_hook_trusted_hash(event, entry, hook)
                prior_state = trusted_by_hash.get(trusted_hash)
                if prior_state is None:
                    continue
                key = f"{source_path}:{HOOK_EVENT_LABELS[event]}:{group_index}:{hook_index}"
                trust_entries.append((key, trusted_hash, prior_state.get("enabled", True)))
    return trust_entries


def decode_toml_key(raw):
    if raw.startswith('"'):
        return json.loads(raw)
    if raw.startswith("'"):
        return raw[1:-1]
    return None


def hook_state_key(line):
    match = re.match(r"""\s*\[hooks\.state\.((?:"(?:\\.|[^"\\])*")|(?:'[^']*'))\]\s*$""", line)
    if not match:
        return None
    return decode_toml_key(match.group(1))


def parse_bool_value(value):
    trimmed = strip_toml_comment(value).strip().lower()
    if trimmed == "true":
        return True
    if trimmed == "false":
        return False
    return None


def parse_string_value(value):
    trimmed = strip_toml_comment(value).strip()
    if trimmed.startswith('"'):
        return json.loads(trimmed)
    if trimmed.startswith("'"):
        end = trimmed.find("'", 1)
        return trimmed[1:end] if end != -1 else trimmed[1:]
    return None


def parse_state_lines(lines):
    values = {}
    for line in lines:
        if "=" not in line:
            continue
        name, value = line.split("=", 1)
        name = name.strip()
        if name == "enabled":
            enabled = parse_bool_value(value)
            if enabled is not None:
                values["enabled"] = enabled
        elif name == "trusted_hash":
            trusted_hash = parse_string_value(value)
            if trusted_hash is not None:
                values["trusted_hash"] = trusted_hash
    return values


def collect_hook_state(text):
    states = {}
    lines = text.splitlines()
    idx = 0
    while idx < len(lines):
        key = hook_state_key(lines[idx])
        if key is None:
            idx += 1
            continue
        idx += 1
        state_lines = []
        while idx < len(lines) and not any_section_header(lines[idx]):
            state_lines.append(lines[idx])
            idx += 1
        states[key] = parse_state_lines(state_lines)
    return states


def strip_hook_state_blocks(text, keys, trusted_hashes=None):
    trusted_hashes = trusted_hashes or set()
    if not keys and not trusted_hashes:
        return text
    lines = text.splitlines()
    output = []
    idx = 0
    while idx < len(lines):
        key = hook_state_key(lines[idx])
        if key is not None:
            header = lines[idx]
            idx += 1
            state_lines = []
            while idx < len(lines) and not any_section_header(lines[idx]):
                state_lines.append(lines[idx])
                idx += 1
            values = parse_state_lines(state_lines)
            if key in keys or values.get("trusted_hash") in trusted_hashes:
                continue
            output.append(header)
            output.extend(state_lines)
            continue
        output.append(lines[idx])
        idx += 1
    if not output:
        return ""
    return "\n".join(output).rstrip() + "\n"


def append_hook_state_entries(text, trust_entries, existing_state):
    if not trust_entries:
        return text
    blocks = []
    for entry in trust_entries:
        if len(entry) == 3:
            key, trusted_hash, enabled = entry
        else:
            key, trusted_hash = entry
            enabled = existing_state.get(key, {}).get("enabled", True)
        lines = [f"[hooks.state.{json.dumps(key)}]"]
        lines.append(f"enabled = {toml_value(enabled)}")
        lines.append(f"trusted_hash = {toml_value(trusted_hash)}")
        blocks.append("\n".join(lines))
    return text.rstrip() + "\n\n" + "\n\n".join(blocks) + "\n"


def update_hook_trust_state(text, trust_entries):
    existing_state = collect_hook_state(text)
    keys = {entry[0] for entry in trust_entries}
    trusted_hashes = {entry[1] for entry in trust_entries}
    text = strip_hook_state_blocks(text, keys, trusted_hashes)
    return append_hook_state_entries(text, trust_entries, existing_state)


config_text = open(config_path).read() if os.path.isfile(config_path) else ""
if inline_hooks_present(config_text):
    hooks_json_config = read_hooks_json()
    migrated_by_event = {}
    for event, entries in hooks_json_config.get("hooks", {}).items():
        for entry in entries if isinstance(entries, list) else []:
            if not is_scribe_hook(entry):
                migrated_by_event.setdefault(event, []).append(entry)

    config_text = strip_scribe_inline_hooks(config_text)
    existing_state = collect_hook_state(config_text)
    existing_inline_by_event = parse_inline_hooks(config_text)
    existing_inline_counts = count_inline_groups(config_text)
    scribe_base_indices = count_inline_groups(config_text)
    migrated_base_indices = dict(existing_inline_counts)
    for event, entries in migrated_by_event.items():
        if event in scribe_base_indices and isinstance(entries, list):
            scribe_base_indices[event] += len(entries)
    config_text = append_inline_entries(config_text, migrated_by_event)
    config_text = append_inline_entries(config_text, scribe_by_event)
    trust_entries = []
    trust_entries.extend(prior_trust_entries_for(config_path, existing_inline_by_event, existing_state))
    trust_entries.extend(prior_trust_entries_for(config_path, migrated_by_event, existing_state, migrated_base_indices))
    trust_entries.extend(scribe_trust_entries_for(config_path, scribe_by_event, scribe_base_indices))
    config_text = update_hook_trust_state(config_text, trust_entries)
    tmp_path = config_path + ".tmp"
    with open(tmp_path, "w") as f:
        f.write(config_text)
    os.replace(tmp_path, config_path)

    if os.path.exists(hooks_path):
        os.remove(hooks_path)
        print(f"  Removed {hooks_path} after migrating hooks into config.toml")
    print(f"  Updated {config_path}")
    print("  Scribe Codex hooks routed via scribe-hook-helper IPC (inline TOML).")
else:
    config = read_hooks_json()
    hooks = config.setdefault("hooks", {})

    for event, scribe_entries in scribe_by_event.items():
        existing = hooks.get(event, [])
        hooks[event] = merge_event_hooks(existing, scribe_entries)

    config["hooks"] = hooks
    write_hooks_json(config)

    config_text = open(config_path).read() if os.path.isfile(config_path) else ""
    existing_state = collect_hook_state(config_text)
    trust_entries = []
    trust_entries.extend(prior_trust_entries_for(hooks_path, hooks, existing_state))
    trust_entries.extend(scribe_trust_entries_for(hooks_path, hooks))
    config_text = update_hook_trust_state(config_text, trust_entries)
    tmp_path = config_path + ".tmp"
    with open(tmp_path, "w") as f:
        f.write(config_text)
    os.replace(tmp_path, config_path)

    print(f"  Updated {hooks_path}")
    print(f"  Updated {config_path}")
    print("  Scribe Codex hooks routed via scribe-hook-helper IPC.")
PYEOF

echo ""
echo "  Done! Restart Codex sessions for hooks to take effect."
