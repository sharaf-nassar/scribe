# Quickstart: AI Hook Channel — Manual Verification

**Phase 1 output for [plan.md](./plan.md). One verification scenario per user story in [spec.md](./spec.md). All scenarios run against a freshly built Scribe with the AI Hook Channel implementation in place.**

## Prerequisites

```sh
# Build the workspace including the new helper crate.
cargo build --release --workspace

# Reinstall hooks (writes new adapter scripts and registers them in the AI tools' settings files).
just setup-claude   # writes ~/.claude/settings.json, ~/.claude/hooks/...
just setup-codex    # writes ~/.codex/config.toml, ~/.codex/hooks.json
just setup-auggie   # writes ~/.augment/...

# Confirm the helper binary is reachable.
ls -l /usr/share/scribe/scribe-hook-helper
# OR for cargo-built (without install):
ls -l target/release/scribe-hook-helper
```

You should NOT need to restart `scribe-server` to pick up the new helper because the helper is invoked fresh per hook event. You DO need to restart any open AI tool processes so they re-read their settings file. **Do not auto-restart the Scribe server** — per CLAUDE.md, server restarts require explicit user approval; ask before running `just restart-server`.

---

## Scenario 1 — US1: Claude Code feature parity restored (P1)

**Goal**: verify `AskUserQuestion` is no longer blocked, that all CC state transitions reach the Scribe tab indicator, and that no hook-originated bytes leak into the AI tool's view.

### 1a. `AskUserQuestion` succeeds

```sh
# In a Scribe tab:
claude
# Inside Claude, ask it to do something that will trigger an AskUserQuestion call,
# e.g. "Ask me what color scheme I prefer using AskUserQuestion."
```

**Expected**:

- The Ink-based picker renders.
- The tab indicator turns yellow (or your configured `waiting_for_input` color).
- After selection, the response reaches the model.
- **No** `PreToolUse:AskUserQuestion hook error: [...] cannot create /dev/tty` message appears.

### 1b. Prompt-bar populates

```sh
# In the same Claude session:
# Submit any prompt.
```

**Expected**:

- The tab indicator turns the `processing` color.
- The prompt bar at the top/bottom of the pane (depending on layout) shows the submitted prompt text (truncated to 256 chars).

### 1c. Stop-hook idle/waiting classification

```sh
# Have Claude finish a turn with a prose question ("Want me to proceed?").
```

**Expected**: tab indicator transitions to `waiting_for_input` (yellow) — the server-side classifier `stop_classifier::classify` matched the "proceed?" phrase pattern.

```sh
# Have Claude finish a turn with no question (just a statement).
```

**Expected**: tab indicator transitions to `idle_prompt` (idle color).

### 1d. Zero leakage

```sh
# Snapshot the Claude pane's visible content after a session that has fired several hooks:
scribe-test screenshot --session <session_id>   # via crates/scribe-test::capture::screenshot
```

**Expected**: no `OSC 1337` byte fragments, no `\e]1337` sequences, no `cannot create /dev/tty` strings, no other hook-originated bytes anywhere in the rendered grid or scrollback.

```sh
# Verify the model's conversation transcript also has no hook leakage:
cat ~/.claude/projects/*/sessions/<latest>.jsonl | jq '.message.content' | grep -E '(SCRIBE|1337|/dev/tty)'
# Expected: no matches.
```

---

## Scenario 2 — US2: Codex / Auggie surface parity (P2)

**Goal**: verify Codex and Auggie state, prompt, and task-label flow identically to Claude via the same channel.

### 2a. Codex state transitions

```sh
# In a fresh Scribe tab:
codex
# Submit a prompt.
```

**Expected**:

- Tab indicator turns `processing`.
- Prompt bar shows submitted text.
- On task completion, tab strip shows the sanitized task label (e.g. "Add user auth").
- After Codex idles, label clears and indicator returns to `idle_prompt`.

### 2b. Auggie state transitions

```sh
# In a fresh Scribe tab:
auggie
# Submit a prompt; let it complete.
```

**Expected**: same indicator + prompt-bar behavior as Codex. The same `scribe-hook-helper` binary handles all three.

### 2c. Context-window indicator (Claude statusline)

```sh
# In a Scribe tab with active Claude session:
# Use Claude until context fill reaches a band threshold (e.g. >30%).
```

**Expected**: Scribe's context indicator updates (the `ContextChanged` event from `dist/ai-hook-statusline.sh` reached the server within 200 ms).

---

## Scenario 3 — US3: Adding a new AI provider (P3)

**Goal**: verify FR-018 — adding a new provider requires only one adapter script.

### 3a. Stub "foo" provider walkthrough (manual exercise, not a real provider)

```sh
# 1. Add 'Foo' to AiProvider enum in crates/scribe-common/src/ai_state.rs
#    (one variant, one id, one display_name).

# 2. Write dist/ai-hook-foo.sh modeled after dist/ai-hook-claude.sh.

# 3. Build and install.
cargo build --release --workspace
sudo install -m 755 dist/ai-hook-foo.sh /usr/share/scribe/

# 4. Manually emit a hook event as 'foo':
SCRIBE_HOOK_SOCK=/run/user/$(id -u)/scribe/server.sock \
SCRIBE_SESSION_ID=<live session UUID from scribe-test session list> \
/usr/share/scribe/scribe-hook-helper \
    --provider=foo \
    --event=state_changed \
    --state=processing
```

**Expected**: the live pane's tab indicator transitions to `processing` without any change to `scribe-hook-helper`, `hook_ingress.rs`, the env-var injection site, or any other adapter.

If FR-018 holds, **this exercise required zero new transport code**.

---

## Scenario 4 — US4: Hooks run safely outside Scribe (P2)

**Goal**: verify the helper exits 0 silently and never affects the AI tool when Scribe is not running.

### 4a. Helper outside Scribe

```sh
# Outside any Scribe pane (e.g. a regular gnome-terminal or SSH session):
unset SCRIBE_HOOK_SOCK SCRIBE_SESSION_ID

# Invoke the helper directly:
/usr/share/scribe/scribe-hook-helper \
    --provider=claude_code \
    --event=state_changed \
    --state=processing
echo "Exit: $?"
```

**Expected**: `Exit: 0`. No stdout. No stderr. No errors. No file or socket activity.

### 4b. Helper with stale env

```sh
# Simulate orphaned env (socket path points nowhere):
SCRIBE_HOOK_SOCK=/tmp/nonexistent.sock \
SCRIBE_SESSION_ID=00000000-0000-0000-0000-000000000000 \
/usr/share/scribe/scribe-hook-helper \
    --provider=claude_code \
    --event=state_changed \
    --state=processing
echo "Exit: $?"
```

**Expected**: `Exit: 0`. No stdout. No stderr.

### 4c. Adapter outside Scribe

```sh
# Simulate Claude Code firing a PreToolUse:AskUserQuestion hook outside Scribe.
# The adapter reads from stdin and execs the helper.
unset SCRIBE_HOOK_SOCK SCRIBE_SESSION_ID

echo '{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","session_id":"abc"}' \
    | /usr/share/scribe/ai-hook-claude.sh
echo "Exit: $?"
```

**Expected**: `Exit: 0`. No stdout. No stderr. No `cannot create /dev/tty` error.

### 4d. Cloud Claude Code

```sh
# In a Scribe pane, set up a cloud session marker (if available):
CLAUDE_CODE_REMOTE=1 claude
# Submit a prompt; trigger hooks.
```

**Expected**: hooks run, but because cloud-side env doesn't have `SCRIBE_HOOK_SOCK`, the helper exits 0 silently on every event. The Scribe-local tab indicator does NOT update (cloud session is not connected to this Scribe instance). The cloud session itself is unaffected.

### 4e. AskUserQuestion in non-Scribe context

```sh
# Reproduce the original regression environment: run Claude Code in any non-Scribe terminal.
# Trigger AskUserQuestion.
```

**Expected**: AskUserQuestion succeeds. There is no `PreToolUse:AskUserQuestion hook error` because the adapter script's exec of the helper exits 0 cleanly with the unset env vars, not 1 from a `printf > /dev/tty` failure.

---

## Cross-cutting checks (run once after all scenarios)

### No `/dev/tty` redirects survive

```sh
grep -RIn '> /dev/tty' /usr/share/scribe/ ~/.claude/settings.json ~/.codex/hooks.json
# Expected: no output. SC-007 holds.
```

### No OSC 1337 AI hook parser code remains

```sh
grep -n 'ClaudeState\|CodexState\|AuggieState' crates/scribe-pty/src/metadata.rs
# Expected: no output. FR-022 holds.

grep -n 'ScribeAiLaunch' crates/scribe-pty/src/metadata.rs
# Expected: still present (pre-arm sentinel retained per FR-023).
```

### Tab indicator latency

```sh
# Using scribe-test:
scribe-test ipc-trace --filter AiStateChanged --duration 60
# In parallel, drive Claude through a state transition.
```

**Expected**: each `AiStateChanged` message arrives within ~50 ms of the hook firing on a quiescent system. The 200 ms p95 target (SC-002) has substantial headroom.

---

## When verification fails

- **Helper exits non-zero**: check `panic = "abort"` is set and panic hook installed; verify `unsafe { libc::exit(0) }` last-resort handler.
- **Helper writes to stderr**: any `eprintln!`, `dbg!`, or `panic!` slipped through; audit for them. Re-run `cargo clippy -W clippy::print_stderr`.
- **AskUserQuestion still blocked**: confirm `~/.claude/settings.json` no longer contains the old `printf > /dev/tty` commands; `just setup-claude` should have rewritten them. If not, the install scripts haven't been rebuilt.
- **State transitions don't reach Scribe**: confirm `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` are set in the AI tool's env (`env | grep SCRIBE` from inside the AI tool's shell or hook).
- **Context indicator stuck**: confirm `dist/ai-hook-statusline.sh` is registered via `~/.claude/settings.json`'s `statusLine` field.
