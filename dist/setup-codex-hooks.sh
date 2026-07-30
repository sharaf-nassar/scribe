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
    if [[ "$HOOK_SOURCE" != /* ]]; then
        if RESOLVED_HOOK_SOURCE=$(cd "$HOOK_SOURCE" 2>/dev/null && pwd -P); then
            HOOK_SOURCE="$RESOLVED_HOOK_SOURCE"
        fi
    fi
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

# Prelude prepended to every embedded Python step so each config write goes
# through the same crash-safe path.
PYTHON_PRELUDE=$(cat << 'PRELUDE_EOF'
import os
import stat
import tempfile


def atomic_write_text(path, text):
    """Replace path's contents via a same-directory temp file and os.replace.

    A crash between the write and the rename leaves the previous file
    untouched, so an interrupted install can never truncate a config.
    """
    path = os.fspath(path)
    directory = os.path.dirname(path) or "."
    fd, tmp_path = tempfile.mkstemp(dir=directory, prefix=".scribe-tmp-")
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        try:
            mode = stat.S_IMODE(os.stat(path).st_mode)
        except FileNotFoundError:
            umask = os.umask(0)
            os.umask(umask)
            mode = 0o666 & ~umask
        os.chmod(tmp_path, mode)
        os.replace(tmp_path, path)
    except BaseException:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
    try:
        dir_fd = os.open(directory, os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(dir_fd)
    except OSError:
        pass
    finally:
        os.close(dir_fd)


def read_text(path):
    """Return the file's contents, or None when it does not exist."""
    path = os.fspath(path)
    if not os.path.isfile(path):
        return None
    with open(path) as handle:
        return handle.read()


def write_text_if_changed(path, text, current):
    """Write text atomically unless it already matches `current`.

    Callers pass the contents they read once at the start of the run, so
    an install that changes nothing skips the write entirely and leaves
    the file's mtime and inode alone.
    """
    if current == text:
        return False
    atomic_write_text(path, text)
    return True
PRELUDE_EOF
)

# Reads a Python program on stdin, prepends the prelude, and runs it.
scribe_python() {
    { printf '%s\n' "$PYTHON_PRELUDE"; cat; } | python3
}

# ── Step 2: Enable Codex hooks and merge Scribe hook entries ─────────────
# config.toml and hooks.json are each read once here, transformed in
# memory, and written at most once at the end of the run, so an install
# that changes nothing leaves both files (mtime and inode included) alone.
scribe_python << 'PYEOF'
import hashlib
import json
import os
import re

hooks_path = os.path.expanduser("~/.codex/hooks.json")
config_path = os.path.expanduser("~/.codex/config.toml")

original_config_text = read_text(config_path)
original_hooks_text = read_text(hooks_path)


def enable_hooks_feature(text):
    """Return (text, dropped_alias) with [features].hooks = true applied."""
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
        return text, False

    hooks_replaced = False
    removed_codex_hooks = False
    next_lines = lines[:features_start + 1]
    for line in lines[features_start + 1:features_end]:
        key = line.split("=", 1)[0].strip()
        if key == "hooks":
            next_lines.append("hooks = true")
            hooks_replaced = True
        elif key == "codex_hooks":
            removed_codex_hooks = True
        else:
            next_lines.append(line)
    if not hooks_replaced:
        next_lines.append("hooks = true")
    lines = next_lines + lines[features_end:]
    text = "\n".join(lines)
    if lines:
        text += "\n"
    return text, removed_codex_hooks


def report(path, written):
    if written:
        print(f"  Updated {path}")
    else:
        print(f"  {path} already up to date")


def find_scribe_install_prefix():
    env = os.environ.get("SCRIBE_INSTALL_PREFIX")
    if env and os.path.isdir(env):
        return env
    for p in (
        "/usr/share/scribe",
        "/usr/share/scribe-dev",
        "/usr/local/share/scribe",
        "/usr/local/share/scribe-dev",
        "/Applications/Scribe.app/Contents/Resources",
        "/Applications/Scribe-Dev.app/Contents/Resources",
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
        {"type": "command", "command": f'"{adapter}" pre_tool_use'},
    ]),
    # One adapter invocation per Codex event: `post_tool_use` and `stop`
    # each carry their state transition *and* the context-percent refresh,
    # which used to be a second `context` registration on the same event.
    ("PostToolUse", None, [
        {"type": "command", "command": f'"{adapter}" post_tool_use', "timeout": 10},
    ]),
    ("Stop", None, [
        {"type": "command", "command": f'"{adapter}" stop', "timeout": 30},
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


def parse_hooks_json(text):
    if text is None:
        return {}
    return json.loads(text)


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


def scribe_state_keys(source_path, hooks_by_event):
    """Hook-state keys the Scribe hooks occupied *before* this run.

    Consolidating Stop/PostToolUse to one adapter each changes both the
    registered command strings and how many groups Scribe owns per event,
    so the trusted-hash blocks from the previous layout match neither the
    new keys nor the new hashes and would otherwise be left behind
    forever. They are stripped by key, which is exact regardless of how
    the old commands were spelled.
    """
    keys = set()
    if not isinstance(hooks_by_event, dict):
        return keys
    for event in HOOK_EVENTS:
        groups = hooks_by_event.get(event, [])
        if not isinstance(groups, list):
            continue
        for group_index, entry in enumerate(groups):
            if not isinstance(entry, dict):
                continue
            for hook_index, hook in enumerate(entry.get("hooks", [])):
                if not isinstance(hook, dict):
                    continue
                if not command_is_scribe(hook.get("command", "")):
                    continue
                keys.add(f"{source_path}:{HOOK_EVENT_LABELS[event]}:{group_index}:{hook_index}")
    return keys


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


def update_hook_trust_state(text, trust_entries, stale_keys=None):
    existing_state = collect_hook_state(text)
    keys = {entry[0] for entry in trust_entries}
    trusted_hashes = {entry[1] for entry in trust_entries}
    text = strip_hook_state_blocks(text, keys | set(stale_keys or ()), trusted_hashes)
    return append_hook_state_entries(text, trust_entries, existing_state)


config_text, removed_codex_hooks = enable_hooks_feature(original_config_text or "")

if inline_hooks_present(config_text):
    hooks_json_config = parse_hooks_json(original_hooks_text)
    migrated_by_event = {}
    for event, entries in hooks_json_config.get("hooks", {}).items():
        for entry in entries if isinstance(entries, list) else []:
            if not is_scribe_hook(entry):
                migrated_by_event.setdefault(event, []).append(entry)

    stale_scribe_keys = scribe_state_keys(config_path, parse_inline_hooks(config_text))
    stale_scribe_keys |= scribe_state_keys(hooks_path, hooks_json_config.get("hooks", {}))
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
    config_text = update_hook_trust_state(config_text, trust_entries, stale_scribe_keys)
    config_written = write_text_if_changed(config_path, config_text, original_config_text)

    removed_hooks_json = original_hooks_text is not None
    if removed_hooks_json:
        os.remove(hooks_path)

    print("  Enabled [features].hooks = true")
    if removed_codex_hooks:
        print("  Removed deprecated Codex hook feature alias")
    if removed_hooks_json:
        print(f"  Removed {hooks_path} after migrating hooks into config.toml")
    report(config_path, config_written)
    print("  Scribe Codex hooks routed via scribe-hook-helper IPC (inline TOML).")
else:
    config = parse_hooks_json(original_hooks_text)
    stale_scribe_keys = scribe_state_keys(hooks_path, config.get("hooks", {}))
    hooks = config.setdefault("hooks", {})

    for event, scribe_entries in scribe_by_event.items():
        existing = hooks.get(event, [])
        hooks[event] = merge_event_hooks(existing, scribe_entries)

    config["hooks"] = hooks
    hooks_text = json.dumps(config, indent=2) + "\n"
    hooks_written = write_text_if_changed(hooks_path, hooks_text, original_hooks_text)

    existing_state = collect_hook_state(config_text)
    trust_entries = []
    trust_entries.extend(prior_trust_entries_for(hooks_path, hooks, existing_state))
    trust_entries.extend(scribe_trust_entries_for(hooks_path, hooks))
    config_text = update_hook_trust_state(config_text, trust_entries, stale_scribe_keys)
    config_written = write_text_if_changed(config_path, config_text, original_config_text)

    print("  Enabled [features].hooks = true")
    if removed_codex_hooks:
        print("  Removed deprecated Codex hook feature alias")
    report(hooks_path, hooks_written)
    report(config_path, config_written)
    print("  Scribe Codex hooks routed via scribe-hook-helper IPC.")
PYEOF

echo ""
echo "  Done! Restart Codex sessions for hooks to take effect."
