# Audit findings triage — Wave 0 baselines

Pre-fix measurements captured at commit `b90c932`, before any Wave 1 or
Wave 2 change lands. Each section records the numbers plus the exact
commands that produced them so the matching after-side measurement can
re-run the same procedure.

## Hook pipeline exec counts

Process-exec cost of the AI hook adapters per tool event, measured with
`strace -f -e trace=execve` around `dist/ai-hook-codex.sh` (and
`dist/ai-hook-claude.sh`) at `b90c932`. Before side of the US5-3
one-interpreter-run-per-event comparison.

### Result: execs per Codex event

A single Codex event can fan out to more than one adapter invocation:
`dist/setup-codex-hooks.sh` registers both `stop`+`context` on `Stop`
and `tool_processing`+`context` on `PostToolUse`. The per-event totals
below sum every adapter invocation the event triggers.

| Codex event | adapter invocations | execve | `python3` | helper | serial wall ms |
| --- | --- | ---: | ---: | ---: | ---: |
| `SessionStart` (startup/resume/clear) | `session_start` | 7 | 1 | 2 | 30.0 |
| `UserPromptSubmit` | `user_prompt_submit` | 10 | 3 | 3 | 77.3 |
| `PermissionRequest` | `permission_request` | 6 | 1 | 1 | 29.9 |
| `PreToolUse` | `tool_processing` | 5 | 0 | 1 | 4.7 |
| `PostToolUse` | `tool_processing` + `context` | 11 | 1 | 2 | 31.8 |
| `Stop` | `stop` + `context` | 14 | 3 | 2 | 81.4 |

Every `execve` observed succeeded — 0 failed PATH probes, because dash
resolves commands by `stat` before exec'ing, so these counts do not move
with `PATH` length.

### Result: execs per adapter invocation

| adapter | invocation | execve | `python3` | helper | mean ms pass 1 | mean ms pass 2 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| codex | `user_prompt_submit` | 10 | 3 | 3 | 77.3 | 74.1 |
| codex | `tool_processing` | 5 | 0 | 1 | 4.7 | 5.9 |
| codex | `stop` | 8 | 2 | 1 | 54.3 | 49.1 |
| codex | `context` | 6 | 1 | 1 | 27.1 | 29.3 |
| codex | `session_start` | 7 | 1 | 2 | 30.0 | — |
| codex | `permission_request` | 6 | 1 | 1 | — | 29.9 |
| claude | `user_prompt_submit` | 8 | 2 | 2 | 52.1 | — |
| claude | `stop` | 8 | 2 | 1 | 52.9 | — |

Claude rows are informational: `dist/setup-claude-hooks.sh` registers one
adapter invocation per event, so per-event totals equal the per-invocation
row.

### Exec composition

Every adapter invocation pays four execs before doing any work, and the
`stop` paths add a fifth:

| exec | why |
| --- | --- |
| `/bin/sh` | the tool spawns the hook as a command string |
| `ai-hook-*.sh` | the adapter itself (`#!/bin/sh`) |
| `/usr/bin/dirname` | helper resolution, `$(dirname "$0")` |
| `/usr/bin/cat` | `PAYLOAD=$(cat)` reads the hook JSON |
| `/usr/bin/mktemp` | `stop` only, for `--last-message-file` |

Beyond that scaffolding, each field extraction is one more `python3`
interpreter start and each emitted hook event is one more
`scribe-hook-helper`:

- codex `user_prompt_submit` — 3 `python3` (`session_id`, `prompt`,
  task-label normalizer) + 3 helper (`state_changed`, `prompt_received`,
  `task_label_changed`).
- codex `stop` — 2 `python3` (`last_assistant_message`, `session_id`) + 1
  helper (`session_stopped`).
- codex `context` — 1 `python3` (rollout transcript tail parse) + 1
  helper (`context_changed`).
- codex `tool_processing` — 0 `python3`, 1 helper (`state_changed`).

So a Codex `Stop` costs 3 interpreter starts spread over 2 adapter
invocations, and 8 of its 14 execs are pure per-invocation scaffolding
(`sh`, adapter, `dirname`, `cat`, each paid twice). `UserPromptSubmit`
costs 3 interpreter starts and 3 helper starts for one logical event.

### Measurement environment

- Commit: `b90c932` (`chore(beads): record GPUI rebuild audit`), adapters
  and `crates/scribe-hook-helper` unchanged between `b90c932` and the
  Wave 0 merge base.
- Helper built at that commit with
  `CARGO_BUILD_JOBS=12 cargo build --release -p scribe-hook-helper`
  (cargo 1.95.0).
- Host: Linux 6.17.0-29-generic, 64 cores, dash 0.5.12-6ubuntu5 as
  `/bin/sh`, Python 3.12.3, strace 6.8. Load average was ~16 (sibling
  builds) during the run: exec counts are exact and load-independent,
  wall-clock means are advisory and are reported for two independent
  passes to show the spread.
- `PATH` pinned to
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`.

### Reproducing (after-side re-run)

1. Stage a prod-deb-shaped install dir: copy `dist/ai-hook-codex.sh`,
   `dist/ai-hook-claude.sh`, and the release `scribe-hook-helper` side by
   side into `$STAGE`, leaving `SCRIBE_HOOK_HELPER` unset so the adapter
   exercises its default sibling resolution.
2. Point `HOME` at a synthetic tree containing a 214,500-byte rollout
   transcript at
   `$HOME/.codex/sessions/2026/07/29/rollout-<ts>-<uuid>.jsonl` whose last
   `event_msg`/`token_count` record carries
   `model_context_window: 272000` and `last_token_usage.total_tokens`
   ~41k, so the `context` adapter reaches its 64 KiB tail parse.
3. Export `SCRIBE_SESSION_ID=<uuid>` and `SCRIBE_HOOK_SOCK=$STAGE/../hook.sock`,
   and run a throwaway Unix-socket listener on that path that accepts,
   drains, and logs one line per connection. The connection count per run
   must equal the helper exec count — that is what proves the emits were
   real rather than fast-failing on connect.
4. Hook payloads on stdin (one JSON file per event): `session_id` plus
   `prompt` (a one-line prompt) for `user_prompt_submit`; `session_id`,
   `tool_name`, `transcript_path` for `post_tool_use`; `session_id`,
   `transcript_path`, and a multi-line `last_assistant_message` for
   `stop`.
5. Count execs, one traced run per case (counts are deterministic; two
   passes agreed exactly):

   ```sh
   strace -f -e trace=execve -o out.trace -- \
       /bin/sh -c "$STAGE/ai-hook-codex.sh stop" < payload-stop.json
   strace -f -e trace=execve -c -o out.summary -- \
       /bin/sh -c "$STAGE/ai-hook-codex.sh stop" < payload-stop.json
   ```

   Total execs = `grep -c 'execve(' out.trace`; helper invocations =
   the same lines filtered on `scribe-hook-helper` and excluding `= -1`.
   Repeat for `user_prompt_submit`, `tool_processing`, `context`,
   `session_start`, `permission_request`, and for the two
   `ai-hook-claude.sh` invocations.
6. Wall time: 20 untraced iterations per case in a loop, mean of the
   loop's total elapsed time, taken after one warm-up invocation per
   case. Take the measurement mutex so no sibling measurement or build
   overlaps.
7. Sum the per-invocation numbers into per-event totals using the
   `SCRIBE_HOOKS` table in `dist/setup-codex-hooks.sh` (re-check that
   table — the after-side may have changed which adapters a Codex event
   fans out to).
