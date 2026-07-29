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

## Search and attach lock/alloc measurements

Cost of the `Term`-lock critical sections that US8 and US2 shrink:
per-keystroke search (`ipc_server.rs:6136-6139` +
`session_manager.rs:780-855`), the inline replay build
(`attach_flow.rs:268-275`), and the reader stall a stopped client
induces (`ipc_server.rs:7432-7447`, `7277-7305`). Before side of
US8-1/US8-2 keystroke timing and US2-3 fan-out.

### Result: search cost per keystroke

`handle_search_request` takes the session `Term` lock, calls
`snapshot_term`, then scans the snapshot — all inside the guard — once
per query edit, with no client-side debounce. Typing a 10-character
query therefore performs 10 full-history snapshots. Numbers below are 10
consecutive keystrokes of the query `connection` at the default
`scrollback_lines = 10_000`, `limit = 256`
(`scribe_client::search::SEARCH_RESULT_LIMIT`).

`size_of::<ScreenCell>()` is 24 B, so a snapshot's payload is
`cols * (rows + scrollback) * 24`.

| geometry | cells/snapshot | alloc/snapshot | allocations | `snapshot_term` ms (mean/median) | `search_snapshot` ms (mean/median) | per keystroke ms (mean/median) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 120x36 | 1,204,320 | 28,903,688 B (27.6 MiB) | 3 | 10.33 / 10.17 | 5.43 / 6.81 | 15.76 / 16.71 |
| 200x50 | 2,010,000 | 48,240,008 B (46.0 MiB) | 3 | 40.52 / 40.44 | 9.56 / 11.75 | 50.08 / 51.72 |

Totals for the whole 10-character query:

| geometry | 10-keystroke wall ms | 10-keystroke alloc | scan share of critical section |
| --- | ---: | ---: | ---: |
| 120x36 | 157.6 | 275.6 MiB | 34.4% |
| 200x50 | 500.8 | 460.1 MiB | 19.1% |

The audit's "~48 MB per clone" claim is confirmed exactly at a
200-column pane: 48,240,008 B allocated per keystroke, in 3 allocations
(the visible-grid `Vec`, the scrollback `Vec`, and the DEC-mode `Vec`).
The whole snapshot is discarded as soon as `search_snapshot` returns.

Match counts per keystroke were `[256, 256, 251, 251, …]` at 200x50: the
one- and two-character prefixes hit the 256-match limit within the first
rows and exit early (0.16 ms scan), every longer prefix scans the full
2,010,000-cell snapshot (11.7–12.9 ms). That is why the scan mean is
well below its median.

The whole critical section runs under the `Term` mutex the PTY reader
needs for `feed_term`, so at 200x50 a 10-character query holds that lock
for ~0.5 s of the session's own output path.

### Result: inline replay build and attach fan-out

`take_session_replay` runs `snapshot_term` + `build_session_replay`
(`snapshot_to_ansi` then `zstd::bulk::compress`) inline on the attach
task — no `spawn_blocking` (#18) — and `attach_prepared_entries` spawns
one such task per attach entry with no dedup and no cap (#17).

| geometry | ansi bytes | zstd bytes | ratio | `snapshot_term` ms | `snapshot_to_ansi` ms | `zstd` ms | inline total ms (mean/median) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 120x36 | 1,431,827 | 112,084 | 12.8x | 13.10 | 5.97 | 3.00 | 22.07 / 21.03 |
| 200x50 | 2,237,823 | 117,604 | 19.0x | 38.42 | 8.80 | 3.87 | 51.10 / 50.19 |

Fan-out wall time, one `Term` per entry at 200x50 / 10,000 scrollback,
on a 4-worker-thread multi-thread runtime:

| concurrent attach entries | wall ms | ms/entry amortised |
| ---: | ---: | ---: |
| 1 | 61.8 | 61.8 |
| 8 | 138.1 | 17.3 |
| 32 | 582.1 | 18.2 |

Each in-flight build holds one 46.0 MiB snapshot plus a 2.1 MiB ANSI
buffer, so 32 uncapped concurrent builds hold ~1.5 GiB of transient
allocation (derived from the measured per-build allocation, not
separately measured as RSS). That product is the LAN-reachable exposure
#17 closes.

### Result: reader stall under a SIGSTOP'd client

`process_pty_chunk` awaits `send_pty_output` — which awaits
`write_message` straight onto the local client socket — *before* it
feeds the bytes into the server `Term`. A client that stops reading
therefore back-pressures the PTY reader task, and through it the child
process, with no timeout, shed, or buffering anywhere on the path.

Reproduced in the functional container with `yes` saturating one pane
and the harness daemon (the attached client) SIGSTOP'd. Two independent
runs:

| phase | run A | run B |
| --- | ---: | ---: |
| PTY drain rate, client running | 32.5 MB/s | 33.8 MB/s |
| PTY bytes accepted after `SIGSTOP` | 58,290 | 106,421 |
| time to full wedge after `SIGSTOP` | 1,016 ms | 1,019 ms |
| PTY bytes read over the next 5.0 s | 0 | 0 |
| PTY drain resumed after `SIGCONT` | 207 ms | 207 ms |

The `yes` child moves from state `R` to `S` blocked in the tty write
path (`wchan = wait_woken`) once the reader stops draining, and no
server thread is blocked — all tokio workers park in `futex_do_wait`,
so the stall is a parked task, not a busy loop. The 6 s observation
window is the harness limit, not a recovery point: nothing in the code
path bounds the wait, and drain resumes only when the client is
continued.

Only the server's `rchar` is usable as a byte counter here. Socket
traffic moves through `recvmsg`/`sendmsg`, which do not update
`rchar`/`wchar`, so the server's `wchar` and the client's `rchar` both
read ~0 while ~100 MB/3 s flows between them; `rchar` on the server is
consequently a clean PTY-only counter.

### Measurement environment

- Commit: `b90c932` (`chore(beads): record GPUI rebuild audit`), checked
  out detached in a dedicated worktree.
- Profile: `--release` (`opt-level = 3`, thin LTO) for every number. The
  plan says "dev build", meaning locally instrumented rather than
  packaged; measuring the unoptimized debug profile would not describe
  what any shipped or dev-deb server does.
- Host: AMD Ryzen Threadripper 3970X (32C/64T), 125 GiB RAM, Linux
  6.17.0-29-generic, rustc 1.95.0, Docker `rust:1.95-trixie` base for
  the container run.
- The measurement mutex was held for the whole run, so no sibling
  baseline measurement or build overlapped.

### Reproducing (after-side re-run)

1. In a worktree at the commit under test, add a throwaway
   `crates/scribe-server/src/baseline_i7923.rs` and declare it from the
   bottom of `ipc_server.rs` as
   `#[cfg(test)] #[path = "baseline_i7923.rs"] mod baseline_i7923;` —
   inside that module so the private `search_snapshot` is reachable. The
   module needs a file-level `#![allow(...)]` for `unsafe_code`,
   `clippy::print_stdout`, and `clippy::unwrap_used`; delete the file
   before committing so the lint-suppression gate stays clean.
2. The module holds a counting `#[global_allocator]` wrapping `System`
   (bytes, call count, peak live delta), a `Dimensions` fixture at
   `cols x rows` with `total_lines = rows + 10_000`, and a generator
   that feeds 10,036 SGR-colored build-log lines through
   `vte::ansi::Processor` into a `Term` built with
   `build_term_config(10_000)`, planting the needle every 40 lines.
3. Search case: one warm-up `snapshot_term`, then for `k` in `1..=10`
   time `snapshot_term` and `search_snapshot(&snapshot, &query[..k], 256)`
   separately, with the allocator counters sampled around the snapshot.
   Query `connection`; geometries 120x36 and 200x50.
4. Replay case: 10 iterations of `snapshot_term`, `snapshot_to_ansi`,
   `build_session_replay`, subtracting one `snapshot_to_ansi` pass from
   the `build_session_replay` timing because it re-encodes internally.
5. Fan-out case: build N independent `Arc<Mutex<Term>>` fixtures and
   `tokio::spawn` one lock+snapshot+`build_session_replay` future each
   on a 4-worker-thread runtime, timing `join`; N in 1, 8, 32.
6. Run with the measurement mutex held:

   ```sh
   CARGO_BUILD_JOBS=12 cargo test --release -p scribe-server --lib \
       baseline_i7923 -- --nocapture --test-threads=1
   ```

7. Reader stall: `cargo build --release -p scribe-server -p scribe-test
   -p scribe-hook-helper`, then
   `docker build -f docker/Dockerfile.func -t scribe-test-func-<tag> .`
   and run a throwaway script under `tests/e2e/func/` via
   `docker run --rm -v ./tests/e2e:/tests -v ./test-output:/output
   scribe-test-func-<tag> /tests/func/<script>.sh`. Never point this at
   the live server — the container is the isolation boundary.
8. That script: `scribe-test send "$SESSION" 'yes <marker>\n'`, sample
   `/proc/<scribe-server pid>/io` `rchar` over 3 s for the healthy-client
   rate, `kill -STOP` the `scribe-test daemon run` pid, sample `rchar`
   every 500 ms for 6 s, record the `yes` process `state`/`wchan` and
   every server thread's `wchan`, then `kill -CONT` and poll `rchar`
   every 200 ms until it advances. Keep the whole script under the
   entrypoint's 30 s `timeout`.
