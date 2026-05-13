#!/bin/bash
# Shared helpers for Scribe's Codex hook scripts.

scribe_codex_read_payload() {
    if [[ -t 0 ]]; then
        printf ''
    else
        cat || true
    fi
}

scribe_codex_tty_path() {
    python3 - <<'PY'
import os

try:
    fd = os.open("/dev/tty", os.O_RDWR)
except OSError:
    raise SystemExit(0)

try:
    print(os.ttyname(fd))
except OSError:
    pass
finally:
    os.close(fd)
PY
}

SCRIBE_CODEX_TTY_PATH="${SCRIBE_CODEX_TTY_PATH:-$(scribe_codex_tty_path)}"
SCRIBE_CODEX_CACHE_DIR="${HOME}/.codex/hooks/.scribe-task-label-cache"
SCRIBE_CODEX_LABEL_CACHE_FILE=""
SCRIBE_CODEX_OWNER_FILE=""

if [[ -n "$SCRIBE_CODEX_TTY_PATH" ]]; then
    SCRIBE_CODEX_CACHE_KEY="$(
        python3 - "$SCRIBE_CODEX_TTY_PATH" <<'PY'
import re
import sys

print(re.sub(r"[^A-Za-z0-9._-]+", "_", sys.argv[1]))
PY
    )"
    SCRIBE_CODEX_LABEL_CACHE_FILE="${SCRIBE_CODEX_CACHE_DIR}/${SCRIBE_CODEX_CACHE_KEY}.thread"
    SCRIBE_CODEX_OWNER_FILE="${SCRIBE_CODEX_CACHE_DIR}/${SCRIBE_CODEX_CACHE_KEY}.owner"
fi

scribe_codex_has_tty() {
    [[ -n "$SCRIBE_CODEX_TTY_PATH" ]]
}

scribe_codex_emit_osc() {
    if ! scribe_codex_has_tty; then
        return
    fi
    printf '\e]1337;%s\a' "$1" > /dev/tty 2>/dev/null || true
}

scribe_codex_payload_field() {
    local payload="$1"
    local field="$2"
    PAYLOAD="$payload" FIELD="$field" python3 - <<'PY'
import json
import os
import sys

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except (json.JSONDecodeError, ValueError):
    sys.exit(0)

value = payload.get(os.environ.get("FIELD", ""))
if isinstance(value, str):
    print(value)
PY
}

scribe_codex_prompt_starts_new() {
    local payload="$1"
    PAYLOAD="$payload" python3 - <<'PY'
import json
import os

try:
    payload = json.loads(os.environ.get("PAYLOAD", "") or "{}")
except (json.JSONDecodeError, ValueError):
    raise SystemExit(0)

prompt = payload.get("prompt")
if not isinstance(prompt, str):
    raise SystemExit(0)

for raw_line in prompt.splitlines():
    first_line = raw_line.strip()
    if first_line:
        print("1" if first_line.startswith("/new") else "0")
        raise SystemExit(0)
print("0")
PY
}

scribe_codex_nearest_codex_pid() {
    python3 - <<'PY'
import os
import subprocess
from pathlib import Path


def proc_name(pid: int) -> tuple[str, str]:
    comm = ""
    argv0 = ""
    try:
        comm = Path(f"/proc/{pid}/comm").read_text(errors="ignore").strip()
    except OSError:
        pass
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
        argv0 = raw.split(b"\0", 1)[0].decode("utf-8", "ignore")
    except OSError:
        pass
    return comm, os.path.basename(argv0)


def is_codex_process(pid: int) -> bool:
    comm, argv0 = proc_name(pid)
    names = {comm, argv0}
    return any(name in {"codex", "codex.exe", "codex-cli"} for name in names if name)


def parent_pid(pid: int):
    try:
        stat = Path(f"/proc/{pid}/stat").read_text(errors="ignore").split()
        return int(stat[3])
    except (OSError, ValueError, IndexError):
        pass
    try:
        out = subprocess.check_output(["ps", "-o", "ppid=", "-p", str(pid)], text=True)
        return int(out.strip())
    except (OSError, ValueError, subprocess.CalledProcessError):
        return None


def ps_proc_name(pid: int) -> tuple[str, str]:
    try:
        out = subprocess.check_output(["ps", "-o", "comm=", "-p", str(pid)], text=True)
    except (OSError, subprocess.CalledProcessError):
        return "", ""
    command = out.strip()
    return os.path.basename(command), os.path.basename(command.split(" ", 1)[0])


pid = os.getpid()
seen = set()
for _ in range(32):
    parent = parent_pid(pid)
    if parent is None:
        break
    if parent <= 1 or parent in seen:
        break
    seen.add(parent)
    if is_codex_process(parent) or any(
        name in {"codex", "codex.exe", "codex-cli"} for name in ps_proc_name(parent) if name
    ):
        print(parent)
        raise SystemExit(0)
    pid = parent
PY
}

scribe_codex_pid_is_codex() {
    local pid="$1"
    [[ -n "$pid" ]] || return 1
    python3 - "$pid" <<'PY'
import os
import subprocess
import sys
from pathlib import Path

try:
    pid = int(sys.argv[1])
except (ValueError, IndexError):
    raise SystemExit(1)

names = []
try:
    names.append(Path(f"/proc/{pid}/comm").read_text(errors="ignore").strip())
except OSError:
    pass
try:
    raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    names.append(os.path.basename(raw.split(b"\0", 1)[0].decode("utf-8", "ignore")))
except OSError:
    pass
try:
    out = subprocess.check_output(["ps", "-o", "comm=", "-p", str(pid)], text=True)
    command = out.strip()
    names.append(os.path.basename(command))
    names.append(os.path.basename(command.split(" ", 1)[0]))
except (OSError, subprocess.CalledProcessError):
    pass

for name in names:
    if name in {"codex", "codex.exe", "codex-cli"}:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

scribe_codex_owner_reset() {
    if [[ -n "$SCRIBE_CODEX_OWNER_FILE" ]]; then
        rm -f "$SCRIBE_CODEX_OWNER_FILE"
    fi
}

scribe_codex_owner_allows_payload() {
    local payload="$1"
    local claim="${2:-0}"
    SCRIBE_CODEX_OWNER_CLAIMED=0

    if ! scribe_codex_has_tty || [[ -z "$SCRIBE_CODEX_OWNER_FILE" ]]; then
        return 0
    fi

    local session_id
    session_id="$(scribe_codex_payload_field "$payload" session_id)"
    if [[ -z "$session_id" ]]; then
        return 0
    fi

    local codex_pid
    codex_pid="$(scribe_codex_nearest_codex_pid)"
    if [[ -z "$codex_pid" ]]; then
        codex_pid="no-codex"
    fi

    mkdir -p "$SCRIBE_CODEX_CACHE_DIR"

    if [[ ! -f "$SCRIBE_CODEX_OWNER_FILE" ]]; then
        if [[ "$claim" != "1" ]]; then
            return 0
        fi
        if (set -o noclobber; printf '%s\t%s\n' "$codex_pid" "$session_id" > "$SCRIBE_CODEX_OWNER_FILE") 2>/dev/null; then
            SCRIBE_CODEX_OWNER_CLAIMED=1
            return 0
        fi
    fi

    local owner_pid=""
    local owner_session=""
    if [[ -f "$SCRIBE_CODEX_OWNER_FILE" ]]; then
        read -r owner_pid owner_session < "$SCRIBE_CODEX_OWNER_FILE" || true
    fi

    if [[ -z "$owner_pid" || -z "$owner_session" ]]; then
        if [[ "$claim" == "1" ]]; then
            printf '%s\t%s\n' "$codex_pid" "$session_id" > "$SCRIBE_CODEX_OWNER_FILE"
            SCRIBE_CODEX_OWNER_CLAIMED=1
            return 0
        fi
        return 0
    fi

    if [[ "$owner_pid" == "$codex_pid" ]]; then
        if [[ "$owner_session" == "$session_id" ]]; then
            return 0
        fi
        return 1
    fi

    if [[ "$owner_pid" =~ ^[0-9]+$ ]] && { ! kill -0 "$owner_pid" 2>/dev/null || ! scribe_codex_pid_is_codex "$owner_pid"; }; then
        if [[ "$claim" == "1" ]]; then
            printf '%s\t%s\n' "$codex_pid" "$session_id" > "$SCRIBE_CODEX_OWNER_FILE"
            SCRIBE_CODEX_OWNER_CLAIMED=1
            return 0
        fi
        return 0
    fi

    return 1
}
