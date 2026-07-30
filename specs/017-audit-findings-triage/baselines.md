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

#### After side: search scan outside the Term lock (bead `scribe-i79.47`)

Measured on 2026-07-30 at worktree base `c5a92b2`. `handle_search_request`
now takes the snapshot under the guard, drops it, and runs
`search_snapshot` on the owned `ScreenSnapshot`, so the scan leaves the
critical section entirely.

The measurement mutex was unavailable — a stale lock directory from
2026-07-29 20:10 was still present and sibling Wave 1 builds kept the
host at load 27-33 of 64 cores throughout — so absolute times here are
not comparable to the quiet-host before side (the 120x36 snapshot costs
27.3 ms against the before side's 10.3 ms; the 200x50 snapshot, 42.0 ms
against 40.5 ms, does reproduce). Both variants therefore run in one
process against the same fixture, with the `before` rows re-running the
pre-fix body as a paired control. Five rounds per cell, medians reported.
Both variants are asserted to produce identical match coordinates for
every prefix, so the reordering is behaviour-preserving by construction.

Uncontended split of the critical section, 10-keystroke query
`connection`, `limit = 256`:

| geometry | variant | lock hold ms (mean/median) | snapshot ms | scan ms | 10-key lock hold ms |
| --- | --- | ---: | ---: | ---: | ---: |
| 120x36 | before (paired control) | 33.45 / 34.64 | 27.28 / 27.61 | 6.17 / 7.46 | 337.5 |
| 120x36 | after | 26.66 / 26.78 | 26.66 / 26.78 | 6.07 / 7.35 | 276.3 |
| 200x50 | before (paired control) | 52.13 / 54.14 | 41.95 / 42.55 | 10.18 / 11.91 | 543.8 |
| 200x50 | after | 40.84 / 40.13 | 40.84 / 40.13 | 9.77 / 11.58 | 401.9 |

The scan is gone from the hold and nothing else moved: per keystroke the
lock hold drops 22.7% at 120x36 and 25.9% at 200x50, exactly the scan's
share, and the scan itself costs the same off the lock as on it. A
10-character query stops holding the `Term` for 61 ms (120x36) and
142 ms (200x50) of what it held before.

Contended rig — the claim the fix is actually about. A PTY-reader-shaped
task feeds one full-width row through `Processor::advance` into the same
`Arc<Mutex<Term>>` on a 1 ms cadence while the 10-keystroke query runs
beside it on a 4-worker runtime:

| geometry | variant | reader feeds | reader wait sum ms | wait max ms | wait p95 ms | query wall ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 120x36 | before (paired control) | 46 | 340.1 | 38.46 | 37.27 | 384.4 |
| 120x36 | after | 78 | 273.5 | 29.84 | 27.87 | 385.8 |
| 200x50 | before (paired control) | 54 | 501.4 | 58.02 | 54.13 | 563.8 |
| 200x50 | after | 110 | 424.2 | 46.68 | 42.55 | 604.3 |

The reader gets through 70% (120x36) and 104% (200x50) more feeds during
the same query, and its worst single stall drops from a full
snapshot-plus-scan to a snapshot. Reader wait p50 is 0.000 ms in every
row: the mutex is uncontended except at the keystrokes, which is what
makes the max and p95 columns readable as "one critical section".

The 200x50 `after` query wall grows 563.8 → 604.3 ms, and that is the
deliberate trade rather than a regression: `tokio::sync::Mutex` is FIFO,
so once the searcher releases the guard between keystrokes it queues
behind the reader it used to starve. Its own hold sum still falls
507.6 → 434.9 ms; the extra ~170 ms is time it spends waiting instead of
blocking the session's output path.

What this item does **not** fix is the residual: the snapshot still runs
under the lock, still allocates 27.6 MiB (120x36) or 46.0 MiB (200x50)
per keystroke in 3 allocations, and still does so once per query edit —
`snapshot_term` is untouched. That residual is US8-2's (150 ms client
debounce plus one snapshot reused across edits while the overlay is
open), and after it lands the per-query hold should be one snapshot
rather than ten.

Reproducing: same shape as the re-run recipe below. Add a throwaway
`crates/scribe-server/src/measure_i7947.rs` and declare it from the
bottom of `ipc_server.rs` as
`#[cfg(test)] #[path = "measure_i7947.rs"] mod measure_i7947;`, so the
private `search_snapshot` is reachable through `super`. It needs a
file-level `#![allow(...)]` for `clippy::unwrap_used` and the numeric
casts, and it writes its tables to `$MEASURE_I7947_OUT` rather than
stdout so no print lint has to be suppressed. It holds a `Dimensions`
fixture at `cols x rows` with `total_lines = rows + 10 000`, the
three-SGR-run build-log generator planting the needle every 40 lines,
and both handler bodies behind one `Variant` enum. Run it with
`CARGO_BUILD_JOBS=12 cargo test --release -p scribe-server --lib measure_i7947 -- --test-threads=1`,
then delete the file before committing so the lint-suppression gate stays
clean.

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

#### After side: per-session sink state machine (bead `scribe-i79.33`)

Re-measured on the same functional container with the *collateral*
question the baseline could not answer: does a stalled client freeze the
OTHER sessions sharing its connection? It did, because the fan-out held
the connection's writer mutex across the inline local socket write, so
every session's reader queued behind the wedged one.

Rig: session A runs `yes`; session B loops "print ~500 lines, then write
a monotonic counter to a file, then sleep 50 ms", so B's progress is
observable from outside the client. The harness daemon (the attached
client) is SIGSTOP'd for 6 s, then continued.

| phase | before (`8ea42b1`) | after |
| --- | ---: | ---: |
| B's heartbeat ticks during the 6 s stop | 4 | 107 |
| B still responsive after `SIGCONT` | run wedged past the 30 s cap | yes |

B advances at its natural rate throughout the stop, and both sessions
resume immediately on `SIGCONT`: the connection's queue shed A's
`PtyOutput` backlog and resynced it with a fresh `SessionReplay`. The
same run also shows the reader never awaiting a local socket — the queue
plus its drain task is now the only writer, and `AttachedSinks` moved to
a std mutex so holding it across a sink await no longer compiles.

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

### After side: attach fan-out dedup, cap, and off-thread encode (US2-3)

Checked on 2026-07-30 at worktree base `dd0c510` (bead `scribe-i79.35`). The
item dedups an `AttachSessions` request by session id, admits at most
`MAX_CONCURRENT_REPLAY_BUILDS = 8` replay builds process-wide, and carries the
`Term` guard into `spawn_blocking` so the snapshot, ANSI encode and zstd pass
leave the runtime's worker threads.

The change landed two commits later, on `6cec57c`. The numbers carry over
unchanged because all three measured components — `snapshot_term`,
`snapshot_to_ansi` and `build_session_replay` — are byte-identical between
`dd0c510` and that base; the intervening `461d95b` touches only the *decode*
side of `screen_replay.rs`.

Both variants were measured in one process against the same fixtures. The host
was running sibling Wave 1 builds throughout (load average ~22 of 64 cores), so
an absolute comparison against the quiet-host before side would not be sound;
the `inline control` rows re-run the pre-fix body as a paired control instead.
They reproduce the before-side fan-out table within noise, which is what makes
the paired rows readable.

#### One build: same work, same cost

| geometry | ansi bytes | zstd bytes | `snapshot_term` ms | `snapshot_to_ansi` ms | `zstd` ms | total mean/median |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 120x36 before | 1 431 827 | 112 084 | 13.10 | 5.97 | 3.00 | 22.07 / 21.03 |
| 120x36 after | 1 702 170 | 50 445 | 15.49 | 6.38 | 3.27 | 25.14 / 22.62 |
| 200x50 before | 2 237 823 | 117 604 | 38.42 | 8.80 | 3.87 | 51.10 / 50.19 |
| 200x50 after | 2 505 299 | 54 275 | 42.90 | 10.21 | 1.26 | 54.37 / 54.50 |

The fix relocates the build, it does not make it cheaper, and the rows say so.
The residual spread is fixture content — the after-side line generator is a
rewrite and lands 12-19 % more encoded bytes — plus the loaded host. A
truecolor-dense 200x50 fixture (12 467 472 ansi bytes) costs 41.41 / 42.81 /
7.88 ms on the same three components, which is the shape the US2-2 ceiling has
to admit.

#### Fan-out, 4-worker runtime (the before side's rig)

| entries | variant | wall ms | ms/entry | max in-flight | peak MiB | worst 1 ms-tick overshoot ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | before | 61.8 | 61.8 | 1 | — | — |
| 1 | inline control | 64.9 | 64.9 | 1 | 56 | 1.3 |
| 1 | capped + `spawn_blocking` | 53.5 | 53.5 | 1 | 56 | 1.1 |
| 8 | before | 138.1 | 17.3 | — | — | — |
| 8 | inline control | 165.1 | 20.6 | 4 | 219 | 142.6 |
| 8 | capped + `spawn_blocking` | 122.1 | 15.3 | 8 | 434 | 1.6 |
| 32 | before | 582.1 | 18.2 | — | — | — |
| 32 | inline control | 595.1 | 18.6 | 4 | 222 | 563.3 |
| 32 | capped + `spawn_blocking` | 462.5 | 14.5 | 8 | 436 | 2.5 |

Wall time improves 12-21 % against the before side *despite* the cap. Four
worker threads were the real limit on the inline path — note that its
in-flight count never exceeds 4, because a build that never yields owns its
worker for the duration — and moving the encode to the blocking pool buys back
more parallelism than the cap spends.

The overshoot column is the cost #18 names. A task sleeping on a 1 ms tick
beside the fan-out runs up to 563 ms late while 32 inline builds hold the four
workers; with the encode on the blocking pool it runs 2.5 ms late. That delay
was being charged to every other session's I/O scheduled on those workers.

#### Fan-out, default worker count (what the server actually runs)

`scribe-server` builds a plain `new_multi_thread` runtime, so on this 64-thread
host nothing throttled the inline path to four concurrent builds:

| entries | variant | wall ms | ms/entry | max in-flight | peak MiB | worst 1 ms-tick overshoot ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 32 | inline control | 362.7 | 11.3 | 32 | 1 595 | 4.8 |
| 32 | capped + `spawn_blocking` | 423.6 | 13.2 | 8 | 436 | 2.0 |

1 595 MiB of measured transient allocation for one 32-session attach, against
the before side's derived ~1.5 GiB — that derivation was right. The cap holds
the peak at 436 MiB and, more to the point, makes it a constant instead of a
function of how many sessions a LAN peer names in one message. The 17 %
wall-time cost at 32 entries on a 64-thread host is the deliberate trade; at
the worker counts a laptop-class host has, the capped path is still the faster
one.

Dedup is absent from these tables because it is a request-shape defect rather
than a throughput one. Before the fix, an `AttachSessions` naming one session
N times ran N builds against that session and re-opened its sink's buffering
window N-1 times mid-replay, so a single small message could multiply every
number above by any N it chose. Two deterministic unit tests carry it instead:
`duplicate_session_ids_collapse_to_one_attach_target` pins the collapse, and
`replay_builds_queue_behind_the_concurrency_cap` pins the admission gate by
holding all eight slots and showing the ninth attach cannot proceed.

#### Reproducing (this after side)

Same shape as the search/attach re-run above. Add a throwaway
`crates/scribe-server/src/measure_i7935.rs` and declare it from the bottom of
`attach_flow.rs` as
`#[cfg(test)] #[path = "measure_i7935.rs"] mod measure_i7935;`, so the private
`REPLAY_BUILD_SLOTS` is reachable through `super`. It needs a file-level
`#![allow(...)]` for `unsafe_code` (the counting `#[global_allocator]`) and
`clippy::unwrap_used`; it writes its tables to a file rather than stdout, so no
print lint has to be suppressed. The module holds a `Dimensions` fixture at
`cols x rows` with `total_lines = rows + 10 000`, two line generators (a
full-width three-SGR-run line reproducing the before side's ~1.1 encoded bytes
per cell, and a dense one changing color every three cells), an in-flight
counter around each build, and a 1 ms heartbeat task on the same runtime. Each
fan-out cell runs three times per variant and reports medians. Run it with
`CARGO_BUILD_JOBS=12 cargo test --release -p scribe-server --lib measure_i7935 -- --test-threads=1`,
then delete the file before committing so the lint-suppression gate stays
clean.

## Per-prompt metadata and client batch stats

Captured at `b90c932` on 2026-07-29 (bead `scribe-i79.24`). Before-side for
US6-1 (single-source title/bell), US6-2 (OSC 7 suppression + branch cache),
US6-5 (config/theme cache for color queries), US3-2 (batch byte cap) and
US3-3 (paced one-burst-per-redraw).

### Rig

Two throwaway processes built from `b90c932` in a detached worktree checkout,
run against an isolated runtime directory so neither the live stable server nor
the installed `scribe-dev` server is touched:

```bash
git switch --detach b90c932
CARGO_BUILD_JOBS=12 cargo build --release -p scribe-server -p scribe-client -p scribe-test
```

Two measurement-only patches were applied to that detached checkout and
discarded afterwards (they are not part of any commit):

1. `scribe-common/src/socket.rs` — `runtime_dir()` honours
   `SCRIBE_MEASURE_RUNTIME_DIR`, so the rig binds its own socket directory
   instead of `/run/user/<uid>/scribe{,-dev}`. Nothing else reads the value; the
   measured code paths are untouched.
2. `scribe-client` — a `measure_batch` module plus four call sites
   (`ipc_bridge::run_drain`, `sync_frames::drain_until_frame`, and the
   `spawn_drain` closure in `main.rs`) that append one JSONL line per applied
   drain batch when `SCRIBE_MEASURE_BATCH` names a file: raw events in the
   batch, raw `PtyOutput` bytes in the batch, coalesced ops, committed sync
   frames drained, and generation bumps (redraws).

Environment for every run: `XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_DATA_HOME`
and `HOME` pointed at scratch directories; `HOME` is a fresh `git init` repo on
branch `baseline-main`; config is a two-key `config.toml` selecting the built-in
`minimal-dark` preset unless a row says otherwise. Socket paths live under
`/tmp/claude-1000/i7924*` because `SUN_LEN` rejects longer ones.

**Server-side counts.** The server ran under
`strace -f -qq -e trace=openat,openat2 -o server.strace -s 300`, with a
`scribe-test share-tap` relaying the only client (`scribe-test daemon`) so every
frame in both directions landed in a JSONL record. Frame counts are
`grep -c '"type":"<Variant>"'` deltas on that record; disk-read counts are
`grep -c` deltas on the strace record (`/.git/HEAD"`, `/scribe/config.toml"`,
`/scribe/themes/`). Each phase is bracketed by a 1.5 s settle so a delta is
attributable to the phase alone; probes are driven with `scribe-test send`.

**Client-side counts.** No tap and no strace (both would distort the firehose).
An isolated server, a `scribe-test daemon` that owns the window and creates the
session, and the real GPUI client joined additively into that window
(`SCRIBE_JOIN_WINDOW`, with `[remote] sharing_mode = "free_for_all"`) so the
daemon can type into the pane the client renders. Display is
`Xvfb :99 -screen 0 1920x1080x24`; the client selected the RTX 3090 Vulkan
adapter.

### Per-prompt and per-sequence metadata frames

One "prompt" is one `true` + Enter round trip through the packaged bash
integration (`dist/shell-integration/bash/scribe.bash`), which emits OSC 133;D,
OSC 7, two OSC 1337s, OSC 2 and OSC 133;A per cycle. `n` is the number of
prompts or sequences in the phase; the OSC phases run their sequences inside a
single command, so each carries exactly one extra prompt cycle whose
contribution is visible in the neighbouring rows.

| Phase | n | TitleChanged | Bell | GitBranch | CwdChanged | `.git/HEAD` opens | `config.toml` reads |
|---|---|---|---|---|---|---|---|
| prompt cycle, cwd = repo root | 20 | 40 (**2.00**/prompt) | 0 | 20 (1.00) | 20 (1.00) | 20 (1.00) | 0 |
| prompt cycle, cwd 4 levels deep | 10 | 20 (2.00) | 0 | 10 (1.00) | 10 (1.00) | 50 (**5.00**) | 0 |
| raw BEL ×10 | 10 | 2 (the prompt) | 20 (**2.00**/BEL) | 1 | 1 | 1 | 0 |
| OSC 2, ST-terminated ×10 | 10 | 22 = 20 (**2.00**/seq) + 2 | 0 | 1 | 1 | 1 | 0 |
| OSC 0, BEL-terminated ×10 | 10 | 22 = 20 (**2.00**/seq) + 2 | 0 | 1 | 1 | 1 | 0 |
| OSC 7, same value ×10 | 10 | 2 (the prompt) | 0 | 11 = 10 (**1.00**/seq) + 1 | 11 (1.00) | 11 (1.00) | 0 |
| OSC 4 probe, indices 0–255 | 256 | 2 (the prompt) | 622 | 1 | 1 | 1 | 256 (**1.00**) |
| OSC 10 + OSC 11 queries ×10 each | 20 | 2 (the prompt) | 0 | 1 | 1 | 1 | **0** |

Observations that the after-side has to reproduce as changed numbers:

- **Two `TitleChanged` frames per title sequence** (US6-1), independent of the
  terminator. A `;`-containing title produces two frames with *different*
  payloads — `printf '\e]2;alpha;beta\e\\'` emitted `title="alpha"` followed by
  `title="alpha;beta"`, so the pair is not even deduplicable by string equality.
- **Two `Bell` frames per BEL byte** (US6-1). A BEL that terminates an OSC is
  consumed as the terminator and produces none, which is why the OSC 0 row shows
  zero bells and the OSC 4 row shows 622: those are the probe's own BEL
  terminators plus the BELs in the server's echoed OSC 4 replies.
- **No OSC 7 last-value suppression** (US6-2): ten identical OSC 7 values cost
  ten `GitBranch` frames, ten `CwdChanged` frames and ten `.git/HEAD` opens.
- **The `.git/HEAD` walk is uncached and linear in depth** (US6-2): 1 open per
  prompt at the repo root, 5 per prompt from a directory four levels below it
  (four `ENOENT` probes plus the hit).
- **OSC 10/11 cost no config read**: the `Term`'s foreground/background entries
  are populated, so only the OSC 4 palette indices miss into the config path.

#### After side: US6-1 bell half (bead `scribe-i79.10`)

Measured on 2026-07-30 in the functional container (`docker/Dockerfile.func`
built from the bead's worktree), with a `scribe-test share-tap` interposed on
the server socket before the `scribe-test daemon` connects, so the JSONL wire
record is the frame log. Ten `printf "\a"` prompt cycles, counted as
`grep -c '"type":"<Variant>"'` deltas on that record:

| Build | BELs | `Bell` frames | `TitleChanged` frames |
|---|---|---|---|
| worktree base (`5edb90e`) | 10 | 20 (2.00/BEL) | 20 (2.00/prompt) |
| single-source Bell | 10 | **10 (1.00/BEL)** | 20 (2.00/prompt) |

The base row reproduces the "raw BEL ×10" row above, and `TitleChanged` stays at
its unfixed 2.00/prompt in both runs — the control that pins the change to the
bell emitter alone. The client's attention request is 1:1 with the frame
(`on_bell_message` queues one entry per `Bell`, `poll_bells` runs each queued
entry through the gate once), so one frame per BEL is one `request_attention`
per BEL.

#### After side: US6-1 title half (bead `scribe-i79.9`)

Measured on 2026-07-30 with the same rig as the bell half: the functional
container (`docker/Dockerfile.func`) built from this bead's worktree, a
`scribe-test share-tap` interposed on the server socket before the
`scribe-test daemon` connects, and `grep -c '"type":"TitleChanged"'` deltas on
the resulting JSONL wire record. Three phases per build, each preceded by a
1.5 s settle: ten `true` prompt cycles through the packaged bash integration,
then ten `printf '\e]2;alpha;beta\e\\'` (semicolon payload, ST-terminated) and
ten `printf '\e]0;plain\a'` inside one command each, so those two phases carry
one extra prompt cycle:

| Build | prompt ×10 | OSC 2 `alpha;beta` ×10 | OSC 0 `plain` ×10 |
|---|---|---|---|
| worktree base (`affa334`) | 20 (2.00/prompt) | 22 = 20 (2.00/seq) + 2 | 22 = 20 (2.00/seq) + 2 |
| single-source `TitleChanged` | **10 (1.00/prompt)** | **11 = 10 (1.00/seq) + 1** | **11 = 10 (1.00/seq) + 1** |

The base rows reproduce the "prompt cycle, cwd = repo root", "OSC 2,
ST-terminated ×10" and "OSC 0, BEL-terminated ×10" rows above, terminator
independence included. The semicolon payload is the sharper result: the base
run's twenty frames are ten `title="alpha"` interleaved with ten
`title="alpha;beta"`, while the fixed run emits ten `title="alpha;beta"` and
nothing else — the interceptor's first-param-only title is gone rather than
deduplicated. Every `TitleChanged` frame is also one `update_live_session`
write-lock on the live-session registry, so the same halving applies there.

### Branch detection with no attached sink

The session emitted OSC 7 at 4 Hz from a background script while the only client
disconnected (`scribe-test daemon stop`), with `strace` still attached:

| Sink state | `.git/HEAD` opens in a 4 s window |
|---|---|
| one sink attached | 15 |
| zero sinks (detached) | 15 |

Branch detection runs at full cost and the result is discarded when the sink set
is empty — the before-side for the detached-session skip in US6-2.

### Config and theme reads per OSC 4 probe

An OSC 4 probe over all 256 palette indices, once with a built-in preset theme
and once with an external theme file (`themes/baseline-external.toml`):

| Theme kind | `config.toml` reads | `themes/*.toml` reads | Disk reads per color query |
|---|---|---|---|
| built-in preset (`minimal-dark`) | 256 | 0 | 1 |
| external theme file | 256 | 256 | **2** |

Every palette query misses the `Term` colour table and re-reads *and re-parses*
the whole config; an external theme adds a second file read and parse. This is
the before-side for US6-5 ("OSC 4 probe = zero disk reads warm").

#### After side: US6-5 config/theme cache (bead `scribe-i79.15`)

Measured on 2026-07-30 from the bead's worktree with the same rig as the before
side: an isolated `scribe-server` under
`strace -f -qq -e trace=openat,openat2`, scratch `HOME`/XDG roots, the
`SCRIBE_MEASURE_RUNTIME_DIR` socket patch, and the same
`themes/baseline-external.toml`. The probe is
`printf '\033]4;%d;?\007' 0 1 … 255` run inside the session, repeated three
times against one long-lived server; counts are `grep -c` deltas bracketing
each repeat. The before column is the same probe against a build of the same
worktree with only the cache commit reverted, so the rig is held constant.

| Theme kind | Build | probe 1 | probe 2 | probe 3 |
|---|---|---|---|---|
| built-in preset | before | 256 config / 0 theme | 256 / 0 | 256 / 0 |
| built-in preset | cached | **1** config / 0 theme | **0 / 0** | **0 / 0** |
| external theme file | before | 256 config / 256 theme | 256 / 256 | 256 / 256 |
| external theme file | cached | **1** config / **1** theme | **0 / 0** | **0 / 0** |

The before rows reproduce the table above exactly (1.00 config read per query,
2.00 disk reads per query with an external theme). Cached, the whole 256-index
probe costs one config read and one theme read the first time the process
answers a colour query, and zero disk reads on every probe after that — the
US6-5 acceptance number.

Invalidation was checked in the same rig with a single-index probe whose reply
is echoed back as text. With `[appearance] theme = "swap"` and ANSI 0 set to
`#010203`, the cold probe cost 1 theme read and answered
`rgb:0101/0202/0303`, the warm probe cost 0 reads and answered the same.
Rewriting ANSI 0 to `#aabbcc` and sending a real `ClientMessage::ConfigReloaded`
made the next probe cost 1 theme read again and answer
`rgb:aaaa/bbbb/cccc` — the cache is dropped by the reload, not stale-pinned for
the life of the server.

### Client drain batches and drained-events-per-redraw

Three workloads in the same session, in the order run. "Events" are raw
`InboundEvent`s folded into one 4 ms / 100-event batch window; "bytes" are the
raw `PtyOutput` bytes in that batch; "frames" are committed synchronized-update
frames popped by `drain_until_frame`; "redraws" are generation bumps.

| Workload | Batches | Events/batch (mean, p50, max) | Bytes/batch (mean, p50, max) | At the 100-event cap | Frames/batch | Redraws/batch |
|---|---|---|---|---|---|---|
| `yes`, 15 s (110 MB) | 12 804 | 99.98, 100, 100 | 8 669, 8 227, 26 317 | **100.0 %** | 1.00 | 1.00 |
| `cat` of a 64 MiB text file | 110 | 53.5, 64, 64 | **623 063, 679 716, 932 878** | 0 % | 1.03 | 1.03 |
| 4 000 `CSI ?2026` frames | 44 | 88.6, 97, 100 | 2 982, 3 300, 3 564 | 45.5 % | **91.0** | 1.05 |

Derived headline numbers:

| Metric | `yes` | `cat` 64 MiB | sync frames |
|---|---|---|---|
| drained events per redraw | **99.96** | 52.09 | 84.72 |
| committed frames per redraw | 1.00 | 1.00 | **87.07** |
| bytes per redraw | 8 668 | 606 522 | 2 852 |

- **US3-2 (batch byte cap).** Under `yes` the batch is count-bounded: every
  batch hits `MAX_BATCH_EVENTS = 100` and carries only ~8.7 KB, because the
  server's `PtyOutput` chunks average 87 bytes. Under `cat` the same window
  is byte-dominated — 64 events of ~11.6 KB each, a mean of 609 KiB and a
  maximum of 911 KiB per batch — 89 % of the 1 MiB cap the fix proposes, with no
  bound in the code today other than `PTY_READ_BUF_SIZE` (64 KiB) × 100 =
  6.4 MiB.
- **US3-3 (paced one-burst-per-redraw).** The synchronized-frame workload is the
  clearest before-side: `drain_all_committed` replays a mean of 91 committed
  frames per batch into the grid but raises only 1.05 redraws, i.e. **87
  committed frames are fully parsed per visible redraw** and 86 of them are
  never shown. `OUTPUT_FRAME_CATCH_UP_THRESHOLD = 4` means even the existing
  `drain_until_frame` collapses them, so the pacing wire-in has to change this
  number, not just the call site.

### Caveats

- The client's perf probe reported `frames=1` for every run: bare `Xvfb` has no
  window manager, so GPUI never entered a repaint cycle. The counters above are
  drain-side (IPC thread) and are unaffected, but they are *not* end-to-end
  paint rates, and the render thread never contended for the `panes` mutex
  during these runs.
- Both a `scribe-test daemon` and the GPUI client were attached during the
  client workloads (the shared-window arrangement is what lets the harness type
  into the rendered pane), so the server fanned every frame out twice. The
  client still received exactly one copy of each frame.
- `SessionReplay` sizes were not measured: the harness deliberately ignores
  `SessionReplay` at `daemon.rs:383-387`, and teaching it to inflate replays is
  its own Wave 1 item. The `cat` row is the largest single-batch evidence
  available at `b90c932`.
- Counts come from one run per phase, not a median of repeats. They are
  integer-exact per unit (2.00 titles, 1.00 branch walks, 1.00 config reads), so
  repeats would not move them; the batch-size distributions are the only rows
  with run-to-run spread.

### After side: OSC 7 last-value suppression (US6-2)

The rig above, re-run twice on the same machine against the same worktree —
once with the `send_metadata_event` CWD dedup reverted, once with it in place.
The "before" column reproduces the `b90c932` numbers exactly, which is what
makes the pair comparable.

| Phase | n | GitBranch before → after | CwdChanged before → after | `.git/HEAD` opens before → after |
|---|---|---|---|---|
| prompt cycle, cwd = repo root | 20 | 20 → **0** | 20 → **0** | 20 → **0** |
| OSC 7, same value ×10 | 10 | 11 → **0** | 11 → **0** | 11 → **0** |
| prompt cycle, cwd 4 levels deep | 10 | 10 → **0** | 10 → **0** | 50 → **0** |
| `cd` back to the repo root | 1 | 1 → 1 | 1 → 1 | 1 → 1 |
| alternating `cd` ×10 | 10 | 9 → 8–9 | 9 → 8–9 | 14 → 12–14 |

A repeated OSC 7 now costs one registry compare and nothing else: no frame, no
`.git/HEAD` walk, no workspace-manager lock. The two `cd` rows are the control
— a directory that really changes still emits, so the suppression is last-value
and not a blanket rate limit. The depth-4 row is the largest win because the
uncached walk there was 5 opens per prompt.

The alternating-`cd` row is the only one with run-to-run spread: its ten `cd`s
are driven on fixed sleeps, so one of them can land outside the phase's settle
window and be counted against the next checkpoint. The suppressed rows are
exact zeros in every run.

The detached-sink and OSC 4 rows are unchanged by this item; they belong to the
branch-cache and config/theme-cache work.

### After side: git-branch cache and detached skip (US6-2)

Measured on 2026-07-30 at worktree base `2b28bfc` (bead `scribe-i79.12`), with
the same isolated-runtime-dir rig: two release `scribe-server` builds differing
only in this item's diff, each run under
`strace -f -qq -e trace=openat,openat2`, against a scratch `$HOME` that is a
`git init` repo on branch `measure-main` with one nested directory. `.git/HEAD`
opens are `grep -c '/\.git/HEAD"'` deltas bracketing each phase.

The OSC 7 phases **alternate** between the repo root and the nested directory
rather than repeating one value: after the last-value suppression above, a
repeated OSC 7 costs zero walks in both builds, so only a directory that really
changes reaches branch detection at all. Twenty alternating reports at 4 Hz
therefore cost 30 opens uncached — ten at the root (1 each) and ten one level
below it (2 each: the `ENOENT` probe plus the hit).

| Phase | before | after |
|---|---|---|
| 20 alternating OSC 7, one sink attached | 30 | 30 |
| 20 alternating OSC 7, zero sinks (detached) | 30 | **0** |
| 20 back-to-back `ListSessions`, session parked | 20 | **1** |
| 3 × 5 `ListSessions`, bursts 6 s apart | 15 | **2** |

The attached row is the control: a session whose directory really changes still
walks on every change, because the cache is a single-entry memo keyed on the
directory it was taken in and an alternating CWD misses it every time. The
detached row is the skip — an empty sink set costs zero walks, where before the
result was computed and discarded. The `ListSessions` rows are the TTL: twenty
reads inside one 5 s window collapse to one walk, and spacing the reads past the
TTL brings the walks back (2 rather than 3 because the first burst still falls
inside the window opened by the preceding phase). Every `SessionList` in both
builds reported `git_branch = "measure-main"`, so the cache never changes the
answer. Phases 1–3 reproduced integer-exact across two runs per build.

### After side: ListSessions branch resolution off registry guards (US6-3)

Measured on 2026-07-30 at worktree base `cb9f80e` (bead `scribe-i79.13`) with
the same isolated-runtime-dir rig: two release `scribe-server` builds differing
only in this item's diff, each run under
`strace -f -qq -e trace=openat,openat2`, against a scratch `$HOME` that is a
`git init` repo on branch `measure-main`. The "before" build is current `main`,
so the per-session branch cache and the detached skip are already in it.

The client is a throwaway binary rather than the `scribe-test` daemon, because
the phases need several panes in one window and a `ListSessions` the harness
cannot issue. It opens one window, creates four panes all launched in the same
directory, drives one OSC 7 per pane (a freshly created session's
`LiveSession.cwd` stays unset until a CWD report lands, and an unset CWD skips
branch resolution entirely), waits past the 5 s branch-cache TTL, then issues
`ListSessions` in the pattern each row names. `.git/HEAD` opens are
`grep -c '/\.git/HEAD"'` deltas bracketing the reads.

| Phase — 4 panes, one window | before | after |
|---|---|---|
| 20 back-to-back `ListSessions`, panes at repo root | 4 | **1** |
| 5 `ListSessions` 6 s apart, panes at repo root | 20 | **5** |
| 20 back-to-back `ListSessions`, panes 4 levels deep | 20 | **5** |

Rows 1 and 2 are the per-pane multiplier. The memo is per session, so four
panes sitting in one repository miss four times per TTL window however many
reads arrive: 4 for a single burst, 4 × 5 for five bursts spaced past the TTL.
Keying the walk on the directory instead collapses each of those into one walk
the whole window shares, which is exactly the 4 → 1 and 20 → 5 seen.

Row 3 separates walks from opens. At depth 4 a single walk costs 5 openats (four
`ENOENT` probes plus the hit), so the before build's four walks are 20 opens and
the after build's one walk is 5. The saving is whole walks, not a cheaper walk.

Lock hold time is not instrumented separately: the change is structural, and the
counted opens are the evidence for it. Every open in the "after" column happens
after both `drop()`s, so no registry or workspace-manager reader-writer guard is
held across any of them; in the "before" column all of them are inside the
guarded region that builds the reply. Every `SessionList` in both builds
reported `git_branch = "measure-main"` for all four panes, so neither the
deferral nor the sharing changes the answer. Row 1 reproduced integer-exact
across two runs per build; rows 2 and 3 were run once per build.

### After side: batch byte cap (US3-2)

Checked on 2026-07-30 at worktree base `087a1e9` (bead `scribe-i79.3`). The item
adds `MAX_BATCH_BYTES = 1 048 576` as a third exit condition on
`collect_batch`, alongside the existing 100-event and 4 ms bounds; nothing else
in the drain changed, so a workload whose batches never reach the new bound
cannot move.

| Workload | max bytes/batch at `b90c932` | share of the 1 MiB cap | batches the cap splits |
|---|---|---|---|
| `yes`, 15 s | 26 317 | 2.5 % | none |
| `cat` of a 64 MiB file | 932 878 | 89.0 % | none, as measured |
| decompressed `SessionReplay` | not measured, tens of MiB | >100 % | every one, drained alone |

**Under `yes` the numbers are unchanged, and that is the expected result.** The
before-side row is count-bounded — 100 % of batches hit `MAX_BATCH_EVENTS` at
~8.7 KB, 40 times under the cap — so batch sizes stay at 99.98 events / 8 669
bytes and drained-events-per-redraw stays at 99.96. The cap is a tail bound on
the drain's worst case, not a throughput change to its common case, and a `yes`
row that *had* moved would mean the cap was mis-sized downward.

Where it binds is the shape the count bound admits without measuring: the `cat`
row's chunks average 11.6 KiB, so a saturated queue that fills the event window
before the 4 ms window carries 1.11 MiB, and #66's worst case —
`PTY_READ_BUF_SIZE` × `MAX_BATCH_EVENTS` — is 6.4 MiB. Both are now ≤ 1 MiB. The
one case the cap cannot bound by splitting is a single event larger than the cap
itself, which is exactly the `SessionReplay` path the before-side caveats say
went unmeasured; those are drained alone rather than rejected, because splitting
a pane's bytes would tear its VTE stream.

Evidence is two deterministic unit tests rather than a rig re-run:
`drain_splits_a_multi_megabyte_backlog_at_the_byte_cap` drains a 2.5 MiB backlog
of 64 KiB frames and asserts ≥ 3 batches, none over the cap, with every byte
delivered — the same backlog is one 2 621 440-byte batch with the cap removed —
and `event_larger_than_the_byte_cap_is_drained_alone` pins the single-event
exception. The Xvfb rig above was not re-run: at the measured batch sizes it
would reproduce the before-side rows by construction, and a debug-build re-run
would not be comparable to a release before-side.

### After side: off-lock VTE parse and O(n) splitter (US3-2, US3-4)

Checked on 2026-07-30 at worktree base `6cec57c` (bead `scribe-i79.4`). Two
independent changes, measured separately because only one of them is a
throughput number.

**US3-4, the splitter.** `SyncUpdateFrameSplitter::split_frames` no longer walks
input a byte at a time through a `Vec::remove(0)` staging buffer; it scans to the
next `ESC`, bulk-copies the marker-free run, and only then tries to match a
marker. Measured in a release build with a throwaway `throughput_probe` module
that ran the pre-change implementation and the new one back to back over the same
buffers, on the same machine as the rows above:

| Workload | Before | After | Speedup |
|---|---|---|---|
| `yes`-shaped 87-byte chunks (the shape the before-side row measured) | 71 MiB/s, 13.43 ns/byte | 1 223 MiB/s, 0.78 ns/byte | **17.2x** |
| 8 KiB marker-free chunks (one coalesced batch) | 81 MiB/s, 11.78 ns/byte | 3 216 MiB/s, 0.30 ns/byte | **39.7x** |
| `CSI ?2026` frames, marker every ~700 bytes | 71 MiB/s, 13.38 ns/byte | 596 MiB/s, 1.60 ns/byte | **8.4x** |

At the before-side `yes` rate (110 MB in 15 s) the splitter alone charged the
drain ~98 ms/s of CPU; it now charges ~5.7 ms/s. Equivalence with the old
implementation was checked by a throwaway differential harness — 50 000
randomized multi-message cases over an alphabet biased toward marker bytes,
comparing emitted frames, `inside_sync`, `opened_sync_update`, the withheld
prefix, and `flush_timed_out` output after every message — plus three unit tests
kept in place for the restart, withheld-prefix and all-escape paths.

**US3-2, the parse off the `panes` lock.** `PaneGrids` now guards only its map;
each entry is an `Arc<PaneGrid>` whose `PaneStream` (queue + grid) and published
`PaneFrame` projection lock separately. `resolve_batch_panes` resolves a batch's
panes under one short registry lock and releases it before any byte is parsed,
and every per-frame read (`pane_content`, `selection_spans`, `sync_ime`,
`sync_split_scroll`, the three scrollbar reads) is served from the projection
rather than the grid. The projection holds an `Arc<Content>` that
`make_content` publishes, so republishing after a parse — and handing a snapshot
to `TerminalElement` — copies no rows.

The Xvfb rig was not re-run, for the reason its own caveat gives: *"the render
thread never contended for the `panes` mutex during these runs"* — bare Xvfb has
no window manager, GPUI never entered a repaint cycle, and the rig reported
`frames=1` throughout. It cannot measure the contention this item removes, and a
debug-build re-run would not be comparable to the release before-side either.
The property is instead pinned by
`a_held_pane_stream_blocks_neither_the_registry_nor_a_paint`, which holds one
pane's stream lock — standing in for a batch mid-parse — and asserts the registry
stays free for another pane's batch and both panes still serve a projection. A
paint-path read that regressed to reaching through the stream would deadlock that
test rather than fail quietly.

### After side: paced one-burst-per-redraw (US3-3)

Checked on 2026-07-30 at worktree base `c5a92b2` (bead `scribe-i79.5`). The drain
no longer empties a pane's frame queue per batch: `apply_pane_op` presents one
committed burst through `sync_frames::present_next_burst` (which is
`drain_until_frame`), and a new `run_frame_pacer` task presents whatever pacing
held back, one burst per pane per 16 ms `REDRAW_INTERVAL` — the same clock
`drive_redraws` repaints on. `SessionReplay` and `ScreenSnapshot` bytes leave the
reader as `InboundEvent::PaneRebuild` rather than `PaneOutput`, so `coalesce`
cannot fold them into the output runs around them and `present_rebuild` applies
them as a burst boundary of their own.

**What the before-side rows become.** The paced drain is deterministic given a
batch's frame count and `OUTPUT_FRAME_CATCH_UP_THRESHOLD = 4`, so the three
recorded workloads bound the change without a re-run:

| Workload | Frames/batch (before) | Presented per drain pass (after) |
|---|---|---|
| `yes`, 15 s | 1.00 | 1 — unchanged; the queue never holds a second frame |
| `cat` of 64 MiB | 1.03 | 1, with the 3 % remainder presented on the pacer's next tick |
| 4 000 `CSI ?2026` frames | 91.0 | 91 — a backlog 23x the catch-up threshold still drains through in one call |

`yes` is the acceptance workload, and it now reads one burst per redraw by
construction rather than by coincidence: the queue empties because it holds
exactly one frame, not because the drain empties it. The synchronized-frame row
is where the 87-committed-frames-per-redraw number came from, and pacing
deliberately does not move it: 4 000 frames in ~176 ms is ~23 kHz of committed
frames against a 60 Hz display, the catch-up threshold exists precisely so a pane
that far behind is caught up in one pass instead of accruing seconds of latency,
and no pacing policy can present fewer frames than the producer emits while every
one of them is still parsed. The remaining win on that row belongs to "skip
invisible intermediate-frame rebuilds" (#64), which drops the *rebuild* work for
frames the pacer passes through rather than the parse.

**Not re-run.** The Xvfb rig cannot observe this property end to end, for the
reason its own caveat gives: bare Xvfb has no window manager, GPUI never entered
a repaint cycle, and the rig reported `frames=1` throughout. Its "redraws" column
counts generation bumps rather than painted frames, so re-running it would only
reproduce the derivation above. The behaviour is pinned instead by
`a_rebuild_is_applied_as_its_own_burst` (the replay boundary, including the
half-open synchronized update that would otherwise swallow a replay),
`coalesce_keeps_a_rebuild_out_of_the_runs_around_it` (the ordering, across the
overflow re-coalescing round trip), and the pre-existing
`caught_up_pane_presents_one_burst_per_redraw` /
`backlog_past_threshold_drains_to_latest_frame` pair, which the wire-in now
actually exercises in production instead of leaving as dead code.

### After side: skip invisible intermediate-frame rebuilds (US3-3)

Checked on 2026-07-30 at worktree base `ad36bd1` (bead `scribe-i79.6`), the
remaining half of the US3-3 row above. Advancing a committed frame and
publishing the snapshot a redraw paints are now two steps on `OutputTarget`:
`DisplayOnlyTerminal::advance_output` parses into the grid and only marks the
snapshot stale, and `publish_content` rebuilds it once per drain pass. Frames
the pacer drains through are therefore still parsed — they are what the grid is
made of — but build no `Content` of their own under the pane lock.

**Rebuilds per pass.** The count is exact and profile-independent, so it is
derived from the frames/batch column recorded above rather than re-measured:

| Workload | Frames per drain pass | `make_content` before | after |
|---|---|---|---|
| `yes`, 15 s | 1.00 | 1.00 | 1.00 — unchanged, nothing to skip |
| `cat` of 64 MiB | 1.03 | 1.03 | 1.00 |
| 4 000 `CSI ?2026` frames | 91.0 | 91.0 | **1.00** |
| `SessionReplay` / `ScreenSnapshot` rebuild | n queued + 1 | n + 1 | **1.00** |

`yes` is the acceptance workload and it is deliberately flat here: pacing
already left it holding exactly one frame per pass, so the only rebuild it ever
paid for is the one that paints. The synchronized-frame row is where the 87
-committed-frames-per-redraw number came from, and it is the row this item
collapses — 91 snapshot builds per batch become 1.

**Cost of the rebuilds removed.** Measured headlessly in a debug build (the
profile `just build` produces; Scribe's own code is unoptimized there) by
draining a 4 000-frame backlog of one-line `CSI ?2026` commits into a real
`DisplayOnlyTerminal`, against the same backlog fed one publish per frame:

| Grid | Whole pass, publish per frame | Whole pass, publish once | Snapshot share |
|---|---|---|---|
| 80×24 | 365 ms | 23.4 ms | 92.7 % |
| 200×50 | 1 975 ms | 23.9 ms | **98.8 %** |

The parse is the 23-24 ms that survives in both columns; everything above it was
snapshot construction for screens no redraw could show, and it scales with cell
count (a 200×50 pane is 10 000 cells per rebuild, ~456 µs each here). The
measurement harness was a throwaway test in the worktree and is not committed;
the behaviour it demonstrated is pinned by
`frames_the_pacer_skips_rebuild_no_content` (six drained-through frames, one
publish; a caught-up pane, one publish per burst; an empty queue, none),
`advancing_frames_holds_the_snapshot_until_published` (the real terminal keeps
the published `Arc<Content>` across advances and catches up in one publish), and
the extended `a_rebuild_is_applied_as_its_own_burst` (a rebuild boundary
publishes once for the queue it clears plus the bytes that replace it).

**Not re-run on the Xvfb rig,** for the reason the pacing entry gives: the rig
reported `frames=1` throughout because bare Xvfb never enters a repaint cycle,
so its drain-side counters cannot observe snapshot cost end to end.

## Per-prompt shell hook cost

Wall time and process creations charged to one no-op prompt cycle by
Scribe's shell integration, per shell, with `terminal.env_persistence`
enabled and disabled, at `b90c932`. Before side of the US4-5 / US4-6
shell-hook-waste comparison and of the US5 helper-transport numbers.

### Result: per-prompt cost

Marginal cost of one extra prompt cycle, median of 3 passes, derived by
differencing a 40-prompt and a 200-prompt scripted loop so shell startup,
integration sourcing, the one-shot baseline emit and teardown all cancel.
`spawns` = `clone`+`clone3`+`vfork`+`fork`, `execs` = `execve`+`execveat`,
both from `strace -f -c` and both exact (integers, reproduced across
passes).

| shell | enabled ms | disabled ms | spawns | execs | OSC-only ms | no-integration ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| bash 5.2.21 | 6.89 | 6.86 | 4 | 0 | 3.01 | 0.02 |
| zsh 5.9 | 4.38 | 4.48 | 2 | 0 | 2.30 | 0.11 |
| fish 3.7.0 | 24.32 | 25.20 | 6 | 6 | 7.03 | 4.02 |
| nu 0.114.1 | 0.99 | 1.01 | 0 | 0 | 0.90 | 0.94 |
| pwsh 7.6.4 | 2.93 | 2.51 | 0 | 0 | 2.54 | 1.25 |

`enabled` and `disabled` are separate runs against separate hook sockets
whose peers ingest and drop the frame respectively. **They are equal by
construction, and that is the finding.** Three independent reasons stack
up at `b90c932`: (a) no shell-visible persistence gate exists, so the
snapshot/diff runs unconditionally (#33, #34); (b) a no-op prompt
produces an empty diff, so the helper is not forked per prompt anyway;
(c) `--event=env-delta` never parses, so no env event reaches the server
in either arm (defect D3 below). US4-5 has to make the `disabled` column
converge on the `OSC-only` column.

`OSC-only` is the same integration truncated immediately before its
`Env-delta capture` section — the cost target for the disabled path.
`no-integration` is `SCRIBE_SHELL_INTEGRATION=0`, the bare-shell floor.

So the env-delta machinery alone costs **3.9 ms and 2 forks** per bash
prompt, **2.1 ms** per zsh prompt, **17.3 ms and 2 forks/execs** per fish
prompt, and **0 ms** on nu and pwsh only because their integrations are
broken (D1, D2).

#### After side: pwsh prompt repaired (bead `scribe-i79.50`)

D2 is fixed: `__Scribe-EmitContext` no longer assigns the read-only
`$host`, so the pwsh prompt runs to completion and every prompt now emits
OSC 133;D, OSC 7, `1337;ScribeContext`, `1337;CodexTaskLabelCleared`,
OSC 2 and OSC 133;A with an empty `$Error`, keeps the user's own prompt
text, and reaches `__Scribe-EmitEnvDelta`. The row is re-measured with
the same rig; the `b90c932` row is re-run alongside it in the same
session so the two are directly comparable.

| pwsh 7.6.4 | enabled ms | disabled ms | spawns | execs | OSC-only ms | no-integration ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| before (`b90c932`, as recorded) | 2.93 | 2.51 | 0 | 0 | 2.54 | 1.25 |
| before (control re-run) | 2.58 | 3.02 | 0 | 0 | 2.56 | 1.02 |
| after (repaired) | 3.70 | 3.76 | 0 | 0 | 3.05 | 1.02 |

The prompt got **~1.1 ms more expensive because it now does its job** —
the four marks the exception used to skip cost ~0.5 ms (`OSC-only` moves
2.56 → 3.05) and reaching the env-delta call costs another ~0.65 ms
(`enabled` − `OSC-only`). That 0.65 ms is the real pwsh figure for the
"env-delta machinery alone" sentence above, which previously read 0 ms
only because the prompt died first.

`enabled` and `disabled` stay equal, so the finding that motivates US4-5
now holds on pwsh for the ordinary reason (no shell-visible persistence
gate) rather than because the prompt threw. Spawns and execs stay 0 —
the snapshot is `[Environment]::GetEnvironmentVariables` and a no-op
prompt yields an empty diff, so the helper is still never forked
per-prompt. Socket connections remain 0 in every arm: D3 is unchanged, so
even the one-shot baseline emit is rejected at clap parse.

The measurement mutex was not available — sibling Wave 1 builds held the
box at load average 12-22 throughout — which is why the control row is
re-run rather than compared against the recorded `b90c932` row. Every
figure is the median of two full passes of 3 differenced pairs each; the
`OSC-only` and `no-integration` controls land within 1% and 18% of their
recorded values respectively, so the ~1.1 ms repair cost is well outside
the noise.

#### After side: nu rows with the integration loading (bead `scribe-i79.49`)

Measured on 2026-07-29 from the bead's worktree with the same rig,
methodology, and 60-variable environment as the before side, under the
`.worktrees/.measure-lock` mutex. The before-side nu row was nushell's
own prompt (D1); these are the first numbers that include Scribe's
script. Prompt cycles counted from Scribe's own OSC 133;D for the three
integration variants and nushell's OSC 133;C for the bare-shell floor.

| shell | enabled ms | disabled ms | spawns | execs | OSC-only ms | no-integration ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| nu 0.114.1 (before, D1) | 0.99 | 1.01 | 0 | 0 | 0.90 | 0.94 |
| nu 0.114.1 (loading) | 14.30 | 13.65 | 4 | 2 | 6.23 | 0.91 |

Per-pass spread was 13.7-14.7 ms enabled and 6.1-6.3 ms OSC-only; the
syscall counts reproduced exactly across the 40- and 200-prompt runs.
The `enabled`/`disabled` columns stay equal for the same reason as every
other shell — there is no shell-visible persistence gate (#33, #34).

The 4 spawns and 2 execs are **two `hostname` externals per prompt**, not
the hook helper: `__scribe-pre-prompt` calls `__scribe-host-name` once
for the OSC 7 authority and `__scribe-emit-context` calls it again for
the 1337 `ScribeContext` payload, and nu adds an output-pump thread per
external (`clone` counts threads). A logging shim on `PATH` confirms the
helper is invoked once for the baseline plus once per *changed* env
snapshot — 3 invocations over a 20-prompt loop, not 20 — so the 8.1 ms
between `OSC-only` and `enabled` is the pure in-process snapshot and
O(N^2) diff that #35 / `scribe-i79.18` targets, and the 5.3 ms between
`no-integration` and `OSC-only` is the two `hostname` forks plus the OSC
writes.

Session startup rises from **23 ms / 1 spawn** to **64 ms** (OSC-only
30 ms, bare shell 23.6 ms) — that delta is the one-shot
`--baseline-ready` emit, which nu now actually reaches. Its
`--added-json=` literal is **4,331 argv bytes** for the 60-variable
environment (bash 4,953, zsh 4,909, fish 4,384, pwsh 5,942), and the two
follow-up deltas are 94 and 91 bytes.

Socket connections on `SCRIBE_HOOK_SOCK` are still **0** as shipped,
because D3 below is unfixed and the real helper rejects
`--event=env-delta` at clap parse. Re-running the same session with a
`PATH` shim that rewrites the token to `--event=env_delta` yields
**3 connections** — one `baseline_ready=true` frame carrying 98 variables
and two per-prompt deltas — which is what proves `__scribe-emit-env-baseline`
is reached. Expect the as-shipped count to become 1-per-session-start
once `scribe-i79.51` lands.

### Result: session startup cost

Time from spawn to first prompt, and process creations over the same
window. This is where the one-shot `--baseline-ready` emit lands.

| shell | enabled ms | spawns | OSC-only ms | spawns | no-integration ms | spawns |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| bash | 195 | 135 | 13.7 | 6 | 9.0 | 2 |
| zsh | 3208 | 4345 | 33.1 | 3 | 30.0 | 1 |
| fish | 110 | 14 | 25.1 | 9 | 24.1 | 6 |
| nu | 23 | 1 | 20.2 | 1 | 23.6 | 1 |
| pwsh | 748 | 27 | 655 | 27 | 548 | 28 |

**zsh pays 3.2 seconds and 4,345 forks before its first prompt.**
`__scribe_json_escape` runs `hex=$(printf '%d' "'$c")` for *every
character* of *every* exported value, and the baseline emit escapes the
whole environment: 4,345 command substitutions for a 4.9 KB payload.
bash's 135 forks are the same function called twice per variable rather
than twice per character.

### Where the per-prompt cost goes

- **bash — 4 forks, 0 execs.** `$(__scribe_urlencode "$PWD")`,
  `$(__scribe_sanitize_context ...)` inside `__scribe_emit_context`
  (2 forks, matching the OSC-only column), plus `$(__scribe_diff_env ...)`
  and the `< <(compgen -e)` process substitution in
  `__scribe_snapshot_env` (2 more).
- **zsh — 2 forks, 0 execs.** Both are the OSC-only substitutions; the
  env snapshot/diff is pure in-process, so its 2.1 ms is CPU, not forks.
- **fish — 6 forks, 6 execs.** `hostname` twice and `basename` once for
  the OSC marks, `fish` itself forks once per prompt, plus **two `seq`
  binaries per prompt** from `for i in (seq 1 $now_count)` /
  `(seq 1 $last_count)` in `__scribe_emit_env_delta`. The remaining
  17.3 ms is the O(N^2) `contains -i` scan the same item (#35) targets.
- **nu — 0 forks.** Scribe's script never loads (D1); the figure is
  nushell's own prompt.
- **pwsh — 0 forks.** The prompt function throws before the env-delta
  call (D2); with that repaired the per-prompt cost is 3.95 ms. Repaired
  by `scribe-i79.50` and re-measured at 3.70 ms — see the after-side
  table above.

### Defects found while measuring

None of these three is in the 82-finding inventory, and each one makes a
Wave 1 acceptance criterion unverifiable as written.

- **D1 — `scribe.nu` does not parse on nu 0.114.1**, so the entire
  Nushell integration (OSC marks *and* env-delta) never loads. First
  error: `nu::parser::missing_positional` at `scribe.nu:26` —
  `hostname | str trim | __scribe-sanitize-context` pipes into a command
  whose `value` is a required positional (same shape at line 49). Fixing
  that exposes a second: `(char esc)` at line 115 is no longer a known
  character name. Reproduce with a plain interactive `nu` under
  `XDG_DATA_DIRS=<scripts>:...` and `TERM_PROGRAM=Scribe`.
  *Fixed by `scribe-i79.49`*, which also had to repair three more nu
  0.114 incompatibilities the first two were masking: `into string` maps
  over a list element-wise rather than joining it, so every snapshot
  aborted on `PATH`; the JSON escaper emitted raw ESC (from the OSC
  sequences the script appends to `PROMPT_INDICATOR`), so the server
  rejected the whole baseline payload; and the four top-level `return`
  guards raise *"Return used outside of custom command or closure"*,
  which printed a red error block at startup for every nu user running
  outside Scribe. See the after-side numbers above.
- **D2 — `scribe.ps1` throws on every prompt.** `__Scribe-EmitContext`
  assigns `$host = __Scribe-HostName`, and `$host` is a read-only
  PowerShell automatic variable: *"Cannot overwrite variable Host because
  it is read-only or constant."* The prompt function dies after OSC 133;D
  and OSC 7, so OSC 1337 `ScribeContext`, `CodexTaskLabelCleared`, the
  OSC 2 title and OSC 133;A are never emitted, PowerShell falls back to
  its `PS>` prompt, and `__Scribe-EmitEnvDelta` (called by the wrapper
  after the inner prompt returns) never runs. **Fixed by
  `scribe-i79.50`** — the local is now `$hostName`; before the fix the
  two `ScribeContext` payloads that did escape carried
  `host=System.Management.Automation.Internal.Host.InternalHost`, the
  stringified automatic `$host` object.
- **D3 — `--event=env-delta` is rejected by the helper in all five
  scripts.** `EventKind` in `crates/scribe-hook-helper/src/main.rs`
  carries `#[clap(rename_all = "snake_case")]`, so the accepted value is
  `env_delta`; every other adapter already uses snake_case
  (`state_changed`, `context_changed`, ...). `Cli::try_parse` fails, the
  helper exits 0 silently per FR-007, and no socket is ever opened. A
  listener on `SCRIBE_HOOK_SOCK` counts 0 connections for the as-shipped
  scripts and 1 per session start once the spelling is corrected.

### Result: helper invocation cost and argv payload

Baseline-emit `--added-json=` literal actually placed on the command
line, for the 60-variable measurement environment:

| shell | argv bytes | helper invocations per session start |
| --- | ---: | ---: |
| bash | 4,953 | 1 |
| zsh | 4,909 | 1 |
| fish | 4,384 (+726 on the first delta) | 2 |
| nu | — | 0 (D1) |
| pwsh | 5,942 | 1 |

`scribe-hook-helper` wall cost, 200 sequential invocations of a
4,561-byte payload after one warm-up:

| invocation | mean ms |
| --- | ---: |
| as shipped (`--event=env-delta`, rejected at parse) | 1.03 |
| corrected spelling, live socket (connect + write) | 1.26 |
| corrected spelling, `SCRIBE_HOOK_SOCK` unset | 1.11 |

So ~1.0 ms of the helper's cost is process start and clap parsing and
only ~0.15 ms is the actual IPC — the relevant number for #26 (helper
resolution and packaging) and #42-#45 (payload transport off argv).

#### After side: payload transport off argv (bead `scribe-i79.44`)

Both halves of US5 measured on the shipped scripts, before side at the
merge base and after side with the same rig. The rig replaces
`SCRIBE_HOOK_HELPER` with a stub that copies its own
`/proc/<pid>/cmdline` and its stdin to disk, or points it at the real
release helper with a Unix-socket sink that records each
length-prefixed frame. Absolute byte counts are not comparable to the
`--added-json=` table above — that one used the 60-variable padded
environment, this one uses the driver's throwaway `HOME` — so read the
before/after pairs, not the magnitudes.

**US5-1, no secrets in argv.** A `SCRIBE_PROBE_SECRET` export, baseline
emit, one PTY-driven shell start each:

| shell | argv payload bytes before | argv payload bytes after | stdin bytes after |
| --- | ---: | ---: | ---: |
| bash | 6,467 | 0 | 6,474 |
| zsh | 6,535 | 0 | 6,542 |
| fish | 6,441 | 0 | 6,448 |
| nu | 7,455 | 0 | 7,324 |
| pwsh | 6,825 | 0 | 6,833 |

`secret_in_argv` is true for all five before and false for all five
after; argv is now four or five fixed tokens (`--provider=system`,
`--event=env_delta`, `--payload-stdin`, and `--baseline-ready` on the
one-shot emit). The Codex `task_label_changed` emit was the adapter's
equivalent exposure — the label is derived from the prompt's first line
— and it moves to stdin the same way.

**US5-2, no silent E2BIG loss.** Same rig with the environment padded by
120 exports of 1,500 bytes, which puts the baseline document at ~185 KiB
— past `MAX_ARG_STRLEN` (128 KiB) but with every individual value well
under it, so the shell itself still execs. Frames counted at the socket
sink, real release helper:

| shell | baseline frames before | baseline frames after | frame bytes after |
| --- | ---: | ---: | ---: |
| bash | 0 | 1 | 189,245 |
| zsh | 0 | 1 | 189,306 |
| fish | 0 | 1 | 189,216 |
| nu | 0 | 1 | 190,018 |
| pwsh | 0 | 1 | 189,599 |

Every after-side frame carries all 120 padded names and the probe
secret. Before the change not one of the five delivered anything: the
emit's `execve` failed with `E2BIG` and the caller discards the exit
status by design, so the loss was completely silent. fish's small
per-prompt delta (140 bytes) was the only thing that got through, which
is exactly why the failure went unnoticed — the feature looked alive.

The adapters fail the same way: with a 200 KiB `prompt`, the before-side
`ai-hook-claude.sh user_prompt_submit` delivered its `state_changed`
event and dropped `prompt_received` entirely, and `ai-hook-codex.sh` did
the same. After, both deliver, with 200,043 bytes on stdin.

**Exec composition.** Measured with the helper resolved as a sibling of
the adapter (so `dirname` still runs), small payload:

| adapter | invocation | execve before | execve after |
| --- | --- | ---: | ---: |
| claude | `user_prompt_submit` | 7 | 7 |
| claude | `stop` | 7 | 6 |
| codex | `user_prompt_submit` | 9 | 9 |
| codex | `stop` | 7 | 6 |
| codex | `tool_processing` | 4 | 4 |
| codex | `session_start` | 6 | 6 |
| codex | `permission_request` | 5 | 5 |

`python3` and helper counts are unchanged everywhere: the payload
builder replaces a field extraction rather than adding one, and the
`printf` that feeds the pipe is a shell builtin. `stop` loses one exec
in both adapters because the `mktemp` hand-off for
`--last-message-file` is gone — the assistant's last message now streams
to the helper and never touches disk. The `/usr/bin/mktemp` row in the
Exec composition table above no longer applies.

### Measurement environment

- Commit: `b90c932` (`chore(beads): record GPUI rebuild audit`).
  `dist/shell-integration/**` and `crates/scribe-hook-helper` are
  unchanged between `b90c932` and the Wave 0 merge base.
- Helper built at that commit with
  `CARGO_BUILD_JOBS=12 cargo build --release -p scribe-hook-helper`
  (cargo 1.95.0).
- Host: Linux 6.17.0-29-generic, 64 cores, strace 6.8, Python 3.12.3.
  Shells: bash 5.2.21, zsh 5.9, fish 3.7.0, nu 0.114.1, pwsh 7.6.4
  (snap, classic confinement).
- Every shell is started on a real PTY with `TERM=xterm-256color`,
  `TERM_PROGRAM=Scribe`, `SCRIBE_SHELL_INTEGRATION=1`,
  `SCRIBE_SESSION_ID=5c21be00-0000-0000-0000-000000000025`,
  `SCRIBE_HOOK_SOCK=<sink>`, and a throwaway `HOME` / `XDG_*` tree
  containing empty `.bashrc` / `.zshrc` / `.zshenv` / `.profile`,
  `nushell/{config,env,login}.nu`, and a `fish/config.fish` that only
  clears `fish_greeting`. Per-shell injection mirrors
  `shell_integration::build_env` / `session_manager::build_shell`
  exactly: `--rcfile` for bash, `ZDOTDIR` for zsh, `XDG_DATA_DIRS` for
  fish and nu, `-NoLogo -NoExit -File` for pwsh.
- **The environment is padded to exactly 60 exported variables** with
  48-byte values, so the snapshot/diff workload is identical across
  shells and reproducible. Real sessions vary; scale linearly with the
  variable count (and, for zsh startup, with total payload *bytes*).
- Load average was ~10-20 from sibling agents. The measurement mutex
  (`.worktrees/.measure-lock`) was held for both passes, so no sibling
  measurement overlapped. Two independent full passes were taken; the
  per-prompt medians agreed within 6% and every syscall count agreed
  exactly.

### Reproducing (after-side re-run)

The driver lives outside the repo (it is a measurement rig, not product
code). Re-create it as follows; every number above comes from it.

1. Build the helper at the commit under test and put it on a `PATH`
   ahead of any installed copy:
   `cargo build --release -p scribe-hook-helper`.
2. Run a throwaway Unix-socket listener on `SCRIBE_HOOK_SOCK` that
   accepts, reads the 4-byte big-endian length prefix plus body, and
   counts connections. Run a second one that reads and discards without
   decoding. The first stands in for
   `terminal.env_persistence.enabled = true`, the second for the
   `hook_ingress.rs` "env_persistence feature disabled; EnvChanged
   dropped" path. The connection count is what proves an emit was real
   rather than fast-failing.
3. Drive each shell on a PTY (`pty.fork` or equivalent):
   1. wait for at least one byte of output followed by 500 ms of
      silence — this is the startup phase, and the wait for output is
      required, because zsh emits nothing at all until after its
      multi-second baseline emit;
   2. write N carriage returns (`\r`, not `\n`: reedline and PSReadLine
      only accept CR as Enter) and wait for N prompt markers plus 500 ms
      of silence — this is the loop phase;
   3. write `exit\r` and reap.
   Answer terminal queries on the master side (`ESC[6n` -> `ESC[1;1R`,
   `ESC[?u`, `ESC[c`, `ESC[>c`); without a DSR reply nu spends ~2 s per
   prompt waiting for one.
4. Count prompt cycles from the output stream to prove the loop really
   ran: OSC 133;A for bash/zsh/fish, OSC 133;D for pwsh (its 133;A is
   unreachable while D2 stands), nushell's own OSC 133;A for nu.
5. Per-prompt cost = (loop time at N=200 - loop time at N=40) / 160,
   median of 3 passes. Startup cost = the startup-phase time. Syscall
   counts: one `strace -f -c -e trace=clone,clone3,vfork,fork,execve,`
   `execveat` run at each N, differenced the same way. Note that `clone`
   also counts thread creation, which is why pwsh's per-prompt spawn
   figure is ~0 with noise rather than exactly 0.
6. Take the measurement mutex; interleaved builds move the wall-clock
   numbers by tens of percent (the syscall counts are load-independent).
7. To reproduce the OSC-only column, copy `dist/shell-integration` and
   truncate all five scripts at their first `Env-delta capture` line.

Two pwsh-specific traps, both hit while re-measuring for `scribe-i79.50`:

- Do not write the N carriage returns as one burst. PSReadLine issues its
  own `ESC[6n` between key reads and crashes ("Oops, something went
  wrong") when it finds a queued `\r` where the CPR reply should be. Send
  one `\r` per observed prompt marker, from the same loop that drains the
  master fd, so the added latency is one read/scan/write turn and is
  identical across arms.
- `/snap/bin/pwsh` refuses to start under `strace` ("snap-confine is
  packaged without necessary permissions ... cap_dac_override"). Drive
  `/snap/powershell/current/opt/powershell/pwsh` directly instead — same
  7.6.4 interpreter, no confinement wrapper.

## AiStateChanged frames per context refresh (US5-4)

Both sides measured on 2026-07-30 for bead `scribe-i79.8`, in the
functional container (`docker/Dockerfile.func`) built from the bead's
worktree at base `1091bd9`. There is no Wave 0 row for this one: the
before side is the same worktree with the equality guard in
`send_ai_context_change` stashed out, so the two images differ by that
hunk alone.

### Rig

`scribe-test share-tap` is interposed on the server socket before any
client connects (`mv server.sock server-upstream.sock`, tap listens on
the canonical path — the arrangement `docker/entrypoint-visual.sh`
uses), so the JSONL wire record is the frame log. The container is
driven with `--entrypoint /bin/bash` because the stock func entrypoint
starts the daemon before a script gets control, which is too late to
interpose.

Each phase sends one compound command into the session shell —
`scribe-hook-helper --event=state_changed --state=processing` followed
by five `--event=context_changed --fill-percent=NN` invocations, then
`read -r` so the shell parks instead of printing a prompt (a returning
OSC 133;A clears the live AI state). Counts are
`grep -c '"type":"AiStateChanged"'` deltas on the record across a 3 s
settle.

| Phase | percentages | before | after |
|---|---|---|---|
| repeat | 42 42 42 42 42 | 6 | **2** |
| distinct | 50 51 52 53 54 | 6 | 6 |

Each phase's first frame is the `state_changed` event itself, so the
refresh contribution is 5 → 1 for the repeat phase: the first refresh
moves `context` from `None` to 42 and the four that repeat 42 produce
no frame at all. The distinct phase is the control that pins the change
to the equality guard — five moving percentages still cost five frames
on both builds, so nothing but a repeat is being dropped.
