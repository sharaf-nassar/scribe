#!/bin/sh
# Scribe — Codex AI hook adapter.
#
# Translates Codex's hook stdin JSON into one or more invocations of
# scribe-hook-helper. Called by Codex's hook system with one positional
# argument identifying which hook event fired.
#
# Contract (per specs/003-ai-hook-channel/contracts/helper-cli.md):
#   - exit 0 in EVERY code path (FR-007)
#   - never write to stdout (FR-008)
#   - never write to stderr (FR-009)
#   - never open /dev/tty (FR-010)
#
# Usage:
#   ai-hook-codex.sh <event-name>
#
# Recognized event-name values: session_start, user_prompt_submit,
# permission_request, pre_tool_use, post_tool_use, stop.
#
# One Codex event costs at most one interpreter start: a single python3 run
# reads the hook JSON once and prints the whole emit plan — one
# `<helper-event> <json-document>` line per helper invocation — which the
# shell then replays. `post_tool_use` and `stop` each carry their state
# transition and the context-percent refresh, so no Codex event needs a
# second adapter registration.
#
# The pre-consolidation event names `tool_processing` and `context` stay
# accepted with their old one-emit behaviour: a package upgrade replaces this
# script before setup-codex-hooks.sh rewrites ~/.codex, and a Codex session
# running against the old registration must not lose its state updates in
# between.

set +e
EVENT_NAME="${1:-}"

# Helper resolution: try explicit env var, then sibling-to-this-script
# for Linux package layouts, then the macOS app-bundle MacOS directory.
# `${0%/*}` is dirname without the exec; it yields `$0` unchanged when the
# invocation carried no directory component.
SELF_DIR="${0%/*}"
[ "$SELF_DIR" != "$0" ] || SELF_DIR="."
HELPER="${SCRIBE_HOOK_HELPER:-$SELF_DIR/scribe-hook-helper}"
[ -x "$HELPER" ] || HELPER="$SELF_DIR/../MacOS/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe/scribe-hook-helper"
[ -x "$HELPER" ] || HELPER="/usr/share/scribe-dev/scribe-hook-helper"
[ -x "$HELPER" ] || { cat >/dev/null 2>&1; exit 0; }

case "$EVENT_NAME" in
    pre_tool_use | tool_processing)
        # A fixed state transition that reads no field of the payload, so
        # this path starts no interpreter at all. stdin is still drained:
        # Codex is blocked writing the hook JSON into our pipe and must not
        # see it close early.
        cat >/dev/null 2>&1
        exec "$HELPER" --provider=codex_code --event=state_changed \
            --state=processing </dev/null >/dev/null 2>&1
        ;;
    session_start | user_prompt_submit | permission_request | post_tool_use | stop | context) ;;
    *)
        cat >/dev/null 2>&1
        exit 0
        ;;
esac

# Value-bearing fields never become arguments: /proc/<pid>/cmdline is
# world-readable, and a single argument is capped at MAX_ARG_STRLEN
# (128 KiB), which turned a long prompt into a silent E2BIG. Only the fixed
# event selector reaches python's argv, and only fixed selectors reach the
# helper's — everything else rides the JSON document on stdin.
if ! PLAN=$(python3 -c '
import json
import os
import re
import sys

EVENT = sys.argv[1] if len(sys.argv) > 1 else ""
TAIL_BYTES = 64 * 1024
CACHE_ENTRIES = 16

try:
    RAW = sys.stdin.read()
except (OSError, ValueError):
    RAW = ""
try:
    PAYLOAD = json.loads(RAW)
except ValueError:
    PAYLOAD = {}
if not isinstance(PAYLOAD, dict):
    PAYLOAD = {}


def text_field(name):
    value = PAYLOAD.get(name)
    return value if isinstance(value, str) else ""


SESSION_ID = text_field("session_id")
if "\n" in SESSION_ID or "\r" in SESSION_ID:
    # One plan line per helper invocation, so a value that could carry the
    # line separator cannot be a conversation id.
    SESSION_ID = ""


def with_session(doc):
    if SESSION_ID:
        doc["conversation_id"] = SESSION_ID
    return doc


def add(plan, event, doc):
    plan.append(event + " " + json.dumps(doc, separators=(",", ":")))


def task_label():
    prompt = PAYLOAD.get("prompt")
    if not isinstance(prompt, str):
        return ""
    first = ""
    for raw in prompt.splitlines():
        stripped = raw.strip()
        if stripped:
            first = stripped
            break
    if not first or first.startswith("/"):
        return ""
    normalized = "".join(ch if ch.isprintable() else " " for ch in first)
    normalized = normalized.replace(";", ",")
    normalized = re.sub(r"\s+", " ", normalized).strip()
    return normalized[:120]


def cache_path():
    """Path of the transcript-tail memo, or "" when nowhere is writable."""
    bases = []
    for name in ("XDG_RUNTIME_DIR", "XDG_CACHE_HOME"):
        base = os.environ.get(name)
        if base and os.path.isdir(base):
            bases.append(base)
    home = os.path.expanduser("~")
    if home:
        bases.append(os.path.join(home, ".cache"))
    for base in bases:
        directory = os.path.join(base, "scribe")
        try:
            os.makedirs(directory, mode=0o700, exist_ok=True)
        except OSError:
            continue
        return os.path.join(directory, "codex-context.json")
    return ""


def load_cache(path):
    if not path:
        return {}
    try:
        with open(path, "rb") as handle:
            cache = json.loads(handle.read().decode("utf-8", "ignore"))
    except (OSError, ValueError):
        return {}
    return cache if isinstance(cache, dict) else {}


def store_cache(path, cache):
    if not path:
        return
    text = json.dumps(dict(list(cache.items())[-CACHE_ENTRIES:]), separators=(",", ":"))
    tmp = path + ".tmp-" + str(os.getpid())
    try:
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            os.write(fd, text.encode("utf-8"))
        finally:
            os.close(fd)
        os.replace(tmp, path)
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass


def resolve_transcript():
    transcript_path = text_field("transcript_path")
    if not transcript_path:
        return ""
    sessions_root = os.path.join(os.path.expanduser("~"), ".codex", "sessions")
    candidate = os.path.abspath(os.path.expanduser(transcript_path))
    try:
        root_real = os.path.realpath(sessions_root)
        candidate_real = os.path.realpath(candidate)
        if os.path.commonpath([root_real, candidate_real]) != root_real:
            return ""
        if not candidate_real.endswith(".jsonl") or not os.path.isfile(candidate_real):
            return ""
    except (OSError, ValueError):
        return ""
    return candidate_real


def parse_tail(path):
    """Percent of the context window in use, from the transcript tail."""
    total = 0
    window = 0
    try:
        with open(path, "rb") as handle:
            handle.seek(0, 2)
            size = handle.tell()
            chunk_size = min(size, TAIL_BYTES)
            handle.seek(size - chunk_size)
            chunk = handle.read(chunk_size)
    except OSError:
        return None
    lines = chunk.splitlines()
    if chunk_size < size:
        lines = lines[1:]
    for raw in reversed(lines):
        try:
            record = json.loads(raw.decode("utf-8", "ignore"))
        except ValueError:
            continue
        if not isinstance(record, dict) or record.get("type") != "event_msg":
            continue
        payload = record.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "token_count":
            continue
        info = payload.get("info")
        if not isinstance(info, dict):
            continue
        model_window = info.get("model_context_window")
        if isinstance(model_window, int) and model_window > 0:
            window = model_window
        usage = info.get("last_token_usage")
        if isinstance(usage, dict):
            used = usage.get("total_tokens")
            if isinstance(used, int) and used > 0:
                total = used
                break
    if total <= 0 or window <= 0:
        return None
    return max(0, min(100, round(100 * total / window)))


def context_fill():
    """Memoized parse_tail, keyed on the transcript file identity.

    Every tool call in a turn fires PostToolUse, but the rollout transcript
    only grows when the model reports usage, so the same 64 KiB tail was
    re-read and re-parsed for a result that could not have changed. A miss
    stores the derived value — including "no usage record", so an
    unparseable tail is not re-read either.
    """
    path = resolve_transcript()
    if not path:
        return None
    try:
        stat = os.stat(path)
    except OSError:
        return None
    stamp = [stat.st_size, stat.st_mtime_ns]
    memo_path = cache_path()
    cache = load_cache(memo_path)
    hit = cache.get(path)
    if isinstance(hit, list) and len(hit) == 3 and hit[:2] == stamp:
        return hit[2] if isinstance(hit[2], int) else None
    pct = parse_tail(path)
    cache.pop(path, None)
    cache[path] = stamp + [pct]
    store_cache(memo_path, cache)
    return pct


def add_context(plan):
    pct = context_fill()
    if pct is not None:
        add(plan, "context_changed", {"fill_percent": pct})


def build(plan):
    if EVENT == "session_start":
        # New Codex session: clear task label, then report idle.
        add(plan, "task_label_cleared", {})
        add(plan, "state_changed", with_session({"state": "idle_prompt"}))
    elif EVENT == "user_prompt_submit":
        # State goes to processing, then derive a task label from the
        # prompt first non-empty line (skipping slash commands).
        add(plan, "state_changed", with_session({"state": "processing"}))
        prompt = text_field("prompt")
        if prompt:
            add(plan, "prompt_received", with_session({"text": prompt}))
        label = task_label()
        if label:
            add(plan, "task_label_changed", {"label": label})
    elif EVENT == "permission_request":
        # Codex is about to ask for approval. Surface the attention state
        # in Scribe without deciding the request for Codex.
        add(plan, "state_changed", with_session({"state": "permission_prompt"}))
    elif EVENT == "post_tool_use":
        add(plan, "state_changed", {"state": "processing"})
        add_context(plan)
    elif EVENT == "stop":
        # The assistant message can be many KiB of model output the user has
        # not seen classified yet; it streams to the helper on stdin, so it
        # neither lands on disk nor in argv.
        message = PAYLOAD.get("last_assistant_message")
        add(
            plan,
            "session_stopped",
            with_session({"last_message": message if isinstance(message, str) else ""}),
        )
        add_context(plan)
    elif EVENT == "context":
        add_context(plan)


LINES = []
try:
    build(LINES)
except Exception:
    # A partial plan is the same degradation the per-field extractions had
    # before consolidation: emit what was derived, drop the rest, exit 0.
    pass
try:
    sys.stdout.write("".join(line + "\n" for line in LINES))
except OSError:
    pass
' "$EVENT_NAME" 2>/dev/null); then
    # No python3 on the box, or it died before printing anything. Fall back
    # to the field-free half of the event so state tracking and Stop
    # classification survive exactly as they did when every extraction
    # failed independently.
    cat >/dev/null 2>&1
    case "$EVENT_NAME" in
        session_start)
            PLAN='task_label_cleared {}
state_changed {"state":"idle_prompt"}'
            ;;
        user_prompt_submit | post_tool_use)
            PLAN='state_changed {"state":"processing"}'
            ;;
        permission_request)
            PLAN='state_changed {"state":"permission_prompt"}'
            ;;
        stop)
            PLAN='session_stopped {"last_message":""}'
            ;;
        *)
            PLAN=""
            ;;
    esac
fi

[ -n "$PLAN" ] || exit 0

printf '%s\n' "$PLAN" | while IFS= read -r line; do
    event="${line%% *}"
    document="${line#* }"
    [ -n "$event" ] || continue
    [ "$event" != "$line" ] || continue
    printf '%s' "$document" \
        | "$HELPER" --provider=codex_code --event="$event" --payload-stdin \
            >/dev/null 2>&1
done

exit 0
