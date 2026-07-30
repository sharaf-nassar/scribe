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
