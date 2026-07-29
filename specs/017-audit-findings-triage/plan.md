# Plan: audit-findings-triage

Implementation plan for turning the 82-finding audit disposition
(spec.md, validated at `b90c932`) into ~45 implementation-ready task
beads across three waves. All Q1-Q7 clarification decisions and the
Spec Review feasibility findings are binding inputs.

## Architecture Approach

The feature is a triage-and-fix program, not a single mechanism, so the
plan clusters the 60 fix-worthy findings (61 with the optional #82) by
*shared mechanism* (per Q1)
and resolves the four architectural decisions the Spec Review flagged as
blocking. Everything else is local, anchored repair work.

### (a) Attach losslessness: per-session sink state machine

The reader loop sends each chunk to attached sinks BEFORE feeding the
shared `Term` (`ipc_server.rs:7436-7447` send, then Term advance), and
attach builds the replay from a Term snapshot before the sink is
installed (`attach_flow.rs:151-215`, install at
`ipc_server.rs:7299-7305`). There is no shared critical section, so:

- **Naive install-first** duplicates output: the new sink receives live
  chunks that the later snapshot will also contain.
- **Naive snapshot-first** (status quo) loses output: chunks emitted
  between snapshot and install go to a sink-less no-op send (#3).

**Chosen design — buffering sink with a committed-byte cursor:**

1. Reader stamps every chunk with a monotonic per-session byte offset;
   the counter advances in the same loop iteration that feeds Term, so
   "offset O committed" ⇔ "Term has consumed bytes ≤ O".
2. Attach installs the sink immediately in a **Buffering** state: the
   fan-out path appends **every sink-bound frame — not just PtyOutput
   chunks — in emission order** into the sink's bounded buffer, each
   entry tagged with the offset current at emission. This matters
   because `process_pty_chunk` also emits `TrimScrollback` and
   `ScrollBottom` on the same sink path (`ipc_server.rs:7443-7457`)
   and those frames are not byte-stamped; replaying a scrollback trim
   or bottom-snap against the wrong snapshot would corrupt state.
3. The snapshot is taken under the Term lock together with the current
   committed offset `O_snap` (one critical section, the only new one).
4. Replay is built (off-lock, `spawn_blocking` per #18) and sent.
5. The sink flushes buffered frames tagged `> O_snap` in emission
   order, drops entries tagged `≤ O_snap` (their effects — including
   any interleaved trim/bottom-snap — are already reflected in the
   snapshot), and flips to **Live**.

This state machine is also where **#21/#58 land**: the Live state sends
through a bounded per-sink queue drained by a dedicated writer task, so
the reader never awaits a local socket inline (#21, folding #20's
inline `framing.rs:71` write and #59's slow-local-sink stall), and the
`AttachedSinks` mutex is only held to snapshot the sender handles —
never across an await (#58, `ipc_server.rs:7300-7304`). Queue overflow
follows Q6: coalesce output, then drop-and-resync via the existing
`RequestSnapshot`, per participant. The same machinery is what makes
the client-side pacing fix meaningful (#64: intermediate frames the
pacer skips must not have been load-bearing for correctness — the
resync path is the safety net), so the plan treats #3/#21/#58/#64 as
one entangled mechanism cluster with #64's client half deferred to the
Wave 2 pacing item.

**Rejected alternatives:** a global "pause the reader during attach"
critical section (reintroduces #21's stall class and couples attach
latency to reader throughput); double-feed with client-side dedup
(wire-visible, violates Q2 additive-only).

### (b) Child-process ownership: Pty stays sole reaper, off-worker

alacritty's `Pty` owns the `Child`; `Pty::drop` sends SIGHUP and does a
**blocking waitpid**. Today it is dropped from async tasks
(`ipc_server.rs:5881,5928-5931,7655`), once under the global
`live_sessions` write guard (#5, #6). A naive raw `waitpid` watcher
task would *steal* the exit status from `Pty::drop` and, worse, reap
the PID early so the subsequent SIGHUP in drop could hit a reused PID
— reintroducing the exact #10 class.

**Chosen ownership model:**

- Introduce a `PtyGuard` wrapper (scribe-server, wrapping — not
  forking — the pinned `alacritty_terminal 0.26.0-rc1` per
  constraints). `PtyGuard::teardown()` moves the inner Pty into
  `tokio::task::spawn_blocking`, where `Drop` (SIGHUP + waitpid) runs
  off any Tokio worker and outside all locks. All existing drop sites
  route through it.
- `PtyGuard::defuse()` reproduces the handoff `ManuallyDrop` path
  (`ipc_server.rs:9027-9029`) so the sender-side defuse before handoff
  is unchanged.
- Exit **detection** (#11) never calls `waitpid`: a per-session watcher
  holds a `pidfd` (opened at spawn) and awaits readability; on exit it
  peeks the status with `waitid(..., WNOWAIT)` — the child stays a
  zombie, its PID cannot be reused, and `Pty::drop`'s later blocking
  waitpid reaps it (now effectively instant, and off-worker anyway).
  The watcher emits `SessionExited` with the real exit code/signal
  (#13).
- Handoff-**inherited** sessions have no pidfd and were reparented when
  the old server exited; per Q2 they are documented as exempt — EOF
  cleanup and `exit_code: None` remain for them.

**Rejected:** raw waitpid watcher (status steal + PID reuse); forking
alacritty to make Drop non-blocking (constraint says wrap, not fork);
a global SIGCHLD handler (races with anything else that spawns, and
tokio's own signal handling).

### (c) Reader cancellation is the prerequisite

The PTY master fd is duplicated three ways, so dropping the Pty never
EOFs the reader — teardown today can neither stop nor bound the reader
(#8, #9), the `JoinHandle` is discarded (#7,
`ipc_server.rs:5693`), and an orphaned reader hot-spins on the closed
clipboard channel (#12, `ipc_server.rs:7344-7400`). Fix: add a
`CancellationToken` + retained `JoinHandle` to `PtyReaderState`
(`ipc_server.rs:741-815`); the reader `select!`s cancellation against
the read. This is the hard prerequisite for #5/#9/#12 — without it,
off-worker teardown has nothing awaitable and the join bound is
meaningless.

**Cancel-vs-EOF race, exactly-once `SessionExited`:** all exit paths
(watcher fires, reader EOF, explicit close cancel) funnel into one
idempotent finalizer gated by a CAS on per-session exit state. The
**pidfd watcher wins** — it is the authoritative emitter carrying the
real code/signal; a reader-EOF arrival after the CAS is a no-op. For
handoff-inherited sessions (no watcher) the EOF path takes the CAS and
emits with `exit_code: None` as today. `SessionExited` therefore fires
exactly once per session in every interleaving.

### (d) Teardown lock-ordering protocol

`finalize_pty_reader` takes `live_sessions.write` +
`workspace_manager.write`; close paths currently drop the Pty (and
would join the reader) under those guards (#6,
`ipc_server.rs:5926-5932`; #9, `ipc_server.rs:5858-5957`,
`main.rs:252-268`). Protocol, applied to every close/exit path:

1. Under the write guards (documented order: `live_sessions` before
   `workspace_manager`, never held across `.await`): remove the session
   entry and *take ownership* of the `PtyGuard`, reader handle, and
   cancellation token.
2. Release both guards.
3. Cancel the token, call `PtyGuard::teardown()` (spawn_blocking),
   await the reader join with the Q7 bound (2 s), then detach + `warn!`
   on timeout.

Ordering inside Wave 1 follows this chain: **reader cancellation
blocks Pty-teardown-off-worker blocks the close-window lock fix.**

## Affected Components

Grouped by crate; anchors are the spec's evidence lines at `b90c932`.

### crates/scribe-server

- `src/ipc_server.rs` — the bulk: reader loop and `PtyReaderState`
  (`741-815, 7320-7400, 7436-7462, 7652-7685`), sink fan-out
  (`1237-1250, 1345-1384, 7279-7305`), close/teardown
  (`5858-5957, 5926-5932, 5693`), handoff SIGHUP (`5835-5844`),
  session cap (`5532`), fresh-session attach (`5538-5623`), viewport/
  resize (`6017-6076, 6096-6114`), search (`6136-6139`), ListSessions
  branch walk (`6360-6419`), env-persist enable (`6993-6998`),
  connection semaphore (`1051-1063, 3789-3799`), AI state broadcast
  (`8675-8698`), title/bell/OSC7/branch (`8548-8568, 8619-8641,
  8707-8832`), color queries (`8446-8465`), handoff defuse
  (`9027-9029`).
- `src/attach_flow.rs` (`118-135, 151-215, 268-275`) — fan-out dedup/
  cap, sink install ordering, spawn_blocking encode.
- `src/screen_replay.rs` (`69-72, 276-337`) — decompression ceiling.
- `src/session_manager.rs` (`342-364, 385-771, 780-855, 1209-1221`) —
  cap reservation, shell detection ordering, search snapshot, restore
  file rendering.
- `src/handoff.rs` (`92, 141, 686`) — additive `HandoffState` field
  for PID start-time identity; version stays 6.
- `src/hook_ingress.rs` (`188-195, 240-256, 302`) — envelope
  bootstrap, prompt truncation point.
- `src/delta.rs` (`15, 19, 32-67`) — payload caps, transport.
- `src/metadata.rs` (`81-111`), `src/bell.rs` (`108-112`),
  `src/ed3_filter.rs` (`64-71`) — single-source title/bell, empty-
  chunk guard (fix lands in `apply_pty_filters`/send guard, not OSC).
- `src/config.rs` (`2075-2109, 2148-2173`) — config/theme cache.
- `src/shell_integration.rs` (`34-63, 132-141`) — scripts-dir lookup
  cache (#82, packaged-only), `XDG_DATA_DIRS` injection.
- `src/framing.rs` (`11, 50, 71`) — write path used by per-sink
  writer tasks; frame size constants referenced by batch caps. Out of
  scope: the #60/#61 `Arc`/pre-framed payload protocol change —
  `framing.rs:50` and `ipc_server.rs:1345-1384` are #60/#61 evidence
  anchors only.
- `Cargo.toml` (`63-67, 93-113`) — deb assets: ship nushell/pwsh
  integration scripts, helper path in prod **and dev** deb variants.

### crates/scribe-client

- `src/main.rs` — queues (`6501-6502, 6731-6785, 8983-8996`), replay
  handling (`7880-7881, 8886-8975`), fixed 120×36 startup grid
  (`8963-8971`), batch drain + VTE parse (`7229-7267, 3937`), bell
  attention (`2926-2928`).
- `src/ipc_bridge.rs` (`47-51, 140-174, 211-230, 334-341`) — batch
  byte cap, empty-frame parse path, `env_envelope_id` minting.
- `src/sync_frames.rs` (`29, 157-202, 230-304`) — wire
  `drain_until_frame`, splitter `remove(0)` fix.
- `src/terminal.rs` (`208-215, 612-631`) — skip `make_content` for
  invisible intermediate frames.
- `src/session_lifecycle.rs` (`63-73`) — replay inflate offload.
- Find overlay input path — 150 ms debounce + snapshot-reuse
  signalling (per Q5).

### crates/scribe-cli and crates/scribe-test

- Both create paths hardcode `env_envelope_id: None` — mint/forward a
  launch envelope id (Q4b).
- `scribe-test/src/daemon.rs` deliberately ignores `SessionReplay`
  (`daemon.rs:383-387`) and observes frames that #3 (buffered flush
  ordering), #22 (fresh-create no longer replays), #73/#74 (no empty
  PtyOutput frames), and #13 (real exit codes) change. SessionReplay
  inflate+apply support is promoted to its own early Wave 1 work item
  ("scribe-test SessionReplay support") that #3/#22/#62 depend on;
  the exit-code and empty-frame observation updates remain in-scope
  collateral of their beads.

### crates/scribe-hook-helper

- `src/main.rs` (`89-126`) — dual-accept transport: old argv contract
  AND new stdin / 0600 temp-file payload, for one release (Q2).

### dist/ (shell scripts, installers, packaging)

- `dist/shell-integration/{bash,zsh,fish,nushell,powershell}` —
  `SCRIBE_HOOK_HELPER` resolution instead of bare PATH lookup
  (`scribe.bash:356-366` et al.), disabled-gate skip
  (`scribe.bash:328-352`), payload transport (`scribe.bash:343-363`),
  fish/nu O(N²) diffs (`scribe.fish:247,270`, `scribe.nu:266,272`),
  nu quoting/multi-line applier (`scribe.nu:205-241`), nu
  `XDG_DATA_DIRS` cleanup, pwsh restore dot-source (`scribe.ps1:
  135-154`), fish restore (`scribe.fish:142-149`).
- `dist/ai-hook-codex.sh` (`36-47, 65-119, 199-232`),
  `dist/ai-hook-claude.sh` (`83-91`) — adapter consolidation, stdin
  transport.
- `dist/setup-codex-hooks.sh` (`57-104, 201-210, 660-715`) — atomic
  writes, single RMW, equality-before-write, trusted-hash migration
  when adapter commands change. `dist/setup-claude-hooks.sh` — same
  atomic-write / idempotency treatment where it writes config.
- `dist/macos/build-dmg.sh` — helper path + shell scripts in the DMG
  layout; `dist/debian` dev-variant parity.

## Data Model

- **Env-envelope id minting** moves to the client side of all three
  create paths (GUI `ipc_bridge.rs:334-341`, `scribe-cli`,
  `scribe-test`): each launch mints an id and sends it in
  `CreateSession` (field is already `Option` on the wire — no
  protocol change). Server-side first-envelope **bootstrap** (#31,
  the "T016 compromise" in `hook_ingress.rs:240-256`) creates the
  envelope on first persist instead of skipping.
- **Envelope GC**: on-disk envelopes whose id is no longer referenced
  by any live window/launch are orphaned once minting moves
  client-side; a server-startup scan deletes orphaned envelopes older
  than 30 days (retention value recorded in the work item).
- **Per-shell restore file**: rendering becomes `ShellKind`-driven —
  POSIX `export`/`unset`, fish `set -gx`/`set -e`, pwsh `.ps1` syntax,
  nu-safe encoding (incl. `'\''` and multi-line values). The file
  **extension must vary** (pwsh refuses to dot-source `.sh`; it needs
  `.ps1`), so `create_session` ordering moves shell detection
  (`session_manager.rs:385-771`) **before**
  `prepare_restore_env_file` (`session_manager.rs:1209-1221`).
- **Git-branch cache**: entries keyed `(session_id, cwd)` →
  branch/None, invalidated on OSC 7 change, 5 s TTL for ListSessions
  reads (Q7). Detached sessions (empty sink set) skip detection
  entirely (#55).
- **Config/theme cache**: parsed config + `resolve_theme` result
  cached for dynamic-color queries, invalidated on config reload
  (#75/#76); an OSC 4 256-color probe hits disk zero times after
  warm-up.
- **Exclusion-set additions**: `SCRIBE_HOOK_HELPER` and the
  persistence gate var join the env-diff exclusion set at
  introduction time (Q4c) — otherwise they re-create #72's
  leak-into-baseline defect.

## API / Interface Changes

**No breaking changes are permitted.** The live server is never
restarted (constitution P7); verification uses the `scribe-dev-server`
identity (separate socket/runtime dir) and the docker e2e harness
(`just e2e-func`).

- **Wire protocol** — all changes additive with `#[serde(default)]`
  (Q2): `SessionExited` gains `exit_code`/`signal` population
  (`exit_code` is already `Option<i32>`; signal terminations get a
  distinct additive field, not overloaded negatives), `CreateSession`'s
  envelope id is already `Option`, inbound-overflow resync reuses the
  existing `RequestSnapshot` (no new frame; client detects its own
  overflow per Q6).
- **Helper CLI** — `scribe-hook-helper` dual-accepts the old argv
  contract AND the new stdin/temp-file transport for one release:
  pre-upgrade shells in live sessions keep the old integration
  functions in memory and will keep exec'ing the old contract until
  the shell restarts. Temp-file transport follows the existing
  env-apply model: `XDG_RUNTIME_DIR`, 0600, unlink-after-read, 60 s
  defensive unlink.
- **Handoff** — `HANDOFF_VERSION` is currently 6 (`handoff.rs:92`);
  receiver accepts N/N-1 (`handoff.rs:686`). Fixes touching
  `HandoffState`: #10 adds an optional child start-time (or pidfd
  seed) field for the identity check — absent on old-sender payloads.
  Handoff-**inherited** sessions are documented as exempt from the new
  child-reap / real-exit-code / PID-identity guarantees; EOF-based
  cleanup and `exit_code: None` remain for them (Q2).
- **Server-injected env vars** — `SCRIBE_HOOK_HELPER` (absolute helper
  path) must resolve correctly in all four layouts: prod deb, dev-deb
  variant, DMG `Contents/MacOS`, and dev builds (target dir). The
  persistence gate var is exported at spawn time only, so shell-side
  snapshot/diff work follows the value current at shell start (new
  shells only). Server-side disable is already live — timers stop and
  envelopes are deleted (existing behavior, preserved); enable
  requires a newly started shell to establish a baseline (Q4a). Both
  vars enter the env-diff exclusion set.

## Testing Strategy

Per constitution P3, each story names a user-reachable verification
path; test code is added only where existing coverage must change
(`scribe-test` harness collateral, `sync_frames.rs` unit tests).

- **Wave 0 baselines (constitution P4 — unrecoverable after fixes):**
  captured at `b90c932` before any Wave 1 merge; all results are
  committed to `specs/017-audit-findings-triage/baselines.md`
  alongside the spec:
  - Hook exec counts: `strace -f -e trace=execve -c` (or `execsnoop`)
    around one Codex Stop/PostToolUse/UserPromptSubmit event via
    `dist/ai-hook-codex.sh`; record execs/event (~8-12 expected).
  - Search: time-per-keystroke and allocation for a 10-char query at
    default scrollback (instrumented timing around
    `session_manager.rs:780-855` in a dev build; ~48 MB/clone claim).
  - Attach/fan-out: replay build+compress wall time inline
    (`attach_flow.rs:268-275`) and reader stall under a SIGSTOP'd
    client.
  - Per-prompt metadata: count of TitleChanged/Bell frames, `.git/HEAD`
    stats, and config reads per prompt / per OSC 4 probe (dev-server
    logs + `strace -e trace=openat` on scribe-dev-server).
  - Client: batch sizes and drained-events-per-redraw under `yes`
    (dev-build counters).
  - Shell hooks: per-prompt wall time and fork count with persistence
    enabled AND disabled, for each of bash/zsh/fish/pwsh/nu (`time`
    around a scripted no-op prompt loop + `strace -f -c` on the
    shell). Wave 1 rewrites these scripts, so this is captured first.
- **US1**: manual scenario — SIGSTOP'd client attached to a `yes`
  pane; other sessions stay live; close a session whose child traps
  SIGHUP (`trap '' HUP; sleep inf`) and confirm no worker/lock stall.
  Real exit codes verified via `exit 42` and `kill -TERM` on
  scribe-dev-server; `just e2e-func` for regressions.
- **US2**: docker e2e attach/reattach under continuous output (no gap,
  no duplicate — buffered-flush ordering observable in
  `scribe-test/src/daemon.rs`); truecolor-dense screen round-trips
  handoff on the dev identity without a blank session; fresh create
  shows no 120×36 shrink/regrow (single SIGWINCH observed by a size
  reporter in the pane).
- **US3**: `yes` firehose on dev client — bounded RSS, paced redraws
  (one burst per redraw), forced overflow triggers visible resync.
  `sync_frames.rs` splitter/pacing unit tests updated in place.
  lat.md fallout: Q3 = wire it in, so `lat.md/test.md:571`
  `drain_until_frame` spec entries **stay valid**; only
  `lat.md/client.md:1052` prose is reconciled to the restored design
  plus the defined `SessionReplay` paced ordering.
- **US4**: on dev-deb + DMG builds, enable persistence; new sessions
  in bash/zsh/fish/pwsh/nu persist and restore an exported var across
  restart-of-session; `scribe-cli` and `scribe-test` create paths
  verified via harness (Q4b makes the harness able to verify this).
  Disable takes effect server-side immediately (persist writes stop,
  envelopes deleted — existing live behavior preserved) while running
  shells keep their hook behavior until restarted; verified on the
  dev identity.
- **US5**: re-run the Wave 0 exec-count command; acceptance states
  before/after numbers. `ls -l /proc/<helper>/cmdline` shows no
  payload. Oversized (>128 KiB) delta arrives instead of silent E2BIG.
- **US6**: dev-server frame log shows one TitleChanged and one Bell
  per sequence incl. `;`-payload titles; repeated OSC 7 causes zero
  branch/registry work; OSC 4 probe causes zero config reads warm.
- **US7**: concurrent-create storm on dev identity stops at cap with
  typed error; handoff cleanup skips SIGHUP when start-time
  mismatches (simulated stale PID). For #24/#25: a scripted
  resize-drag / repeated viewport-report scenario against the
  dev-identity server measures the apply rate (TIOCSWINSZ + reflow
  applies) and must settle to ≤4 applies/sec during continuous drag.
- **US8**: keystroke timing before/after in find overlay; one snapshot
  per overlay-open+output-invalidation, not per keystroke.
- **US9**: run `setup-codex-hooks.sh` twice — second run leaves
  mtime/inode unchanged; `kill -9` mid-write leaves valid config.

### lat.md synchronization targets

Wave 1 invalidates more lat.md prose than the pacing sections alone.
Each bead names its target sections from this table and `lat check`
gates every bead (constitution P7).

| Work item(s) | lat.md sections to update |
|---|---|
| Reader cancellation; orphaned-reader fix; close-path lock protocol | `lat.md/server.md#Sessions#PTY Reader Task` (invalidates "reader task exits naturally on EOF") |
| Pty teardown via PtyGuard; child-exit watcher | `lat.md/server.md#Sessions#PTY Reader Task` (invalidates "rely on Pty::Drop to send SIGHUP"); `lat.md/protocol.md#Session Events` (real exit codes/signals) |
| Per-session sink state machine; fresh-session create | `lat.md/server.md#Sessions#Detach and Reattach` |
| Streamed replay decompression | `lat.md/server.md#Handoff#Session Replay Encoding`, `lat.md/server.md#Handoff#Size Limits` |
| Handoff PID-identity check; PtyGuard defuse | `lat.md/server.md#Handoff#Defuse Strategy` |
| Envelope minting/GC; enable/disable semantics; gate var | `lat.md/server.md#Env Persistence#Runtime Enable/Disable Transitions` |
| Helper resolution; payload transport; restore rendering; shell-hook waste | `lat.md/server.md#Shell Integration` |
| Git-branch cache; ListSessions off guards | `lat.md/server.md#Sessions#Git Branch Detection` |
| Pacing wire-in; harness SessionReplay support | `lat.md/client.md:1052` pacing prose; `lat.md/protocol.md#SessionReplay`; `lat.md/test.md` drain_until_frame specs (stay valid per Q3) |
| Single-source TitleChanged/Bell; empty-frame guard | `lat.md/pty.md#Metadata Parser` |

## Risks

- **Decompression ceiling on an untrusted LAN path (#4)**: raising the
  bound naively creates a decompression-bomb vector from LAN peers.
  Mitigation: Q7 — streamed decode with a 64 MiB absolute post-inflate
  ceiling (matches `MAX_MESSAGE_SIZE`), never peer-declared-size-only
  (P5).
- **Helper transport migration window (#42-45)**: pre-upgrade shells
  hold old functions in memory; a hard argv cutover silently kills env
  persistence and AI state for them. Mitigation: dual-accept for one
  release (Q2); removal is a follow-up bead gated on release notes.
- **Handoff one-shot risk**: handoff changes cannot be re-tested
  against the live server (P7) and a bad `HandoffState` change bricks
  upgrade. Mitigation: additive `#[serde(default)]` only, N/N-1 accept
  window preserved, exercised on the dev identity + docker e2e before
  any release. Revert criterion: a failed decode of prior-version
  state in an upgrade rehearsal against a dev-identity server drops
  the new field and relies on the serde default.
- **Codex installer trusted-hash invalidation (#37)**: consolidating
  adapter commands changes the command strings the installer
  hash-registered; already-installed hooks would stop being trusted.
  Mitigation: the consolidation bead includes installer migration that
  re-registers hashes and acceptance criteria for previously
  registered hooks.
- **#1+#2 coupling**: fixing the dead cap (#1) alone activates the
  reservation race (#2). Mitigation: one bead, one landing —
  permit/semaphore pattern mirroring `MAX_CONNECTIONS`, counting
  handoff-restored sessions (`restore_from_handoff` bypasses
  reservation today).
- **Perf regressions from new bounds**: 1 MiB batch cap, 256/1024
  queue bounds, 8-build attach cap could regress latency or drop
  under-provisioned defaults. Mitigation: Q7 numbers are defaults
  beads may refine with recorded rationale; Wave 0 baselines make
  regressions measurable; oversize events process alone rather than
  being rejected.
- **Sink state machine destabilizes the hottest path**: the reader
  fan-out is the throughput core; the buffering/cursor change touches
  every byte. Mitigation: land behind the existing e2e visual suite
  (`just e2e-func`) plus the US2 no-gap/no-dup harness assertions;
  the resync path (Q6) is the correctness backstop; #3/#21/#58 land
  as one reviewed unit rather than three partial edits. Revert
  criterion: any e2e-visual regression or output-ordering corruption
  in the daemon.rs assertions reverts to the install-after-replay
  order behind a flag while the cursor design is reworked.
- **Pacing behavior change (#62)**: wiring `drain_until_frame` reverses
  de-facto behavior. Mitigation: Q3 approved it; `SessionReplay`
  ordering is defined in the pacing bead (replay is applied as its own
  burst boundary — never interleaved inside a committed-frame run);
  lat.md/spec 016 stay authoritative.

## Sequencing

Waves per Q1. Every fix-worthy finding appears in exactly one item:
the disposition table yields 60 Fix=yes findings plus the optional
#82 — 61 total, fully mapped in the Traceability section. Folds per
the table: 20→21, 40→38, 50→48, 51→49, 59→58, 63→62, 66→65, 67→68,
74→73, 76→75, 80→78/79, 81→82. Dependency edges are stated as
"X blocks Y"; each item carries the stable `US<n>-<m>`
acceptance-criterion ids it covers (defined under Traceability).
Final work-item count: **48** (Wave 0: 4, Wave 1: 23, Wave 2: 21).

### Wave 0 — baselines (block their measured areas' fixes)

All measurements captured at `b90c932` and committed to
`specs/017-audit-findings-triage/baselines.md` alongside the spec.

- **Baseline: hook pipeline exec counts** — execs per Codex/Claude
  event, helper invocation count. Blocks "helper payload transport
  off argv" and "Codex adapter consolidation".
- **Baseline: search and attach lock/alloc measurements** — search
  per-keystroke cost, replay build inline wall time, reader stall
  repro. Blocks "search scan outside the Term lock", "search debounce
  and snapshot reuse", "attach fan-out dedup, cap, and off-thread
  encode", and "per-session sink state machine".
- **Baseline: per-prompt metadata and client batch stats** —
  title/bell/branch/config-read counts, batch sizes and
  drained-events-per-redraw under `yes`. Blocks the Wave 2 US6 items,
  "batch byte cap", and "wire drain_until_frame pacing".
- **Baseline: per-prompt shell hook cost** — wall time and forks per
  prompt, persistence enabled AND disabled, per shell (bash/zsh/fish/
  pwsh/nu). Wave 1's "helper resolution and packaging" and "helper
  payload transport off argv" rewrite the same five scripts, making
  this baseline unrecoverable afterward — it blocks those two items
  and the Wave 2 shell-hook-waste items ("persistence gate var and
  disabled-path skip", "fish/nu linear-time env diffs").

### Wave 1 — data loss + security + availability

Harness prerequisite:

- **scribe-test SessionReplay support** — `daemon.rs:383-387`
  deliberately ignores `SessionReplay`; teach the harness to inflate
  and apply replays so replay ordering and content are observable.
  Blocks "per-session sink state machine" (#3 buffered-flush
  ordering), "fresh-session create skips replay" (#22), and Wave 2
  "wire drain_until_frame pacing" (#62 paced replay ordering).

Server lifecycle chain (US1):

- **Reader cancellation and retained handles** (#7, #8; US1-3) —
  token + handle in `PtyReaderState`, reader select!s cancel vs read;
  the exactly-once exit CAS funnel lands here. Blocks "Pty teardown
  off-worker", "orphaned-reader fix", and "close-path lock protocol".
- **Pty teardown off-worker via PtyGuard** (#5; US1-1) — wrapper with
  `teardown()` (spawn_blocking drop) and `defuse()` (ManuallyDrop
  parity for handoff). Blocks "close-path lock protocol" and
  "child-exit watcher" (watcher relies on teardown reaping zombies).
- **Child-exit watcher with real exit codes** (#11, #13; US1-2) —
  pidfd + `waitid(WNOWAIT)` peek, authoritative `SessionExited`
  emitter, additive signal field; inherited sessions exempt;
  daemon.rs exit-code observation update in-scope.
- **Handoff PID-identity check before SIGHUP** (#10; US7-2) —
  start-time (or pidfd) validation; additive `HandoffState` field;
  inherited/absent ⇒ skip-signal-and-log. Revert criterion: failed
  decode of prior-version state in a dev-identity upgrade rehearsal
  drops the field (serde default).
- **Orphaned-reader hot-spin and descendant-held-slave exit** (#12;
  US1-3) — clipboard-channel spin fix; exit detection decoupled from
  EOF (uses the watcher). Blocked by "reader cancellation" and
  "child-exit watcher".
- **Close-path lock protocol and bounded reader join** (#6, #9;
  US1-1, US1-3) — take-then-release-then-join protocol, 2 s bound
  then detach+log, documented lock order. Blocked by "reader
  cancellation" and "Pty teardown off-worker".

Sink/attach mechanism cluster (US1+US2; the sink state machine covers
#3, #21-fanout, #58):

- **Per-session sink state machine** (#3, #21 [20 folded], #58 [59
  folded]; US1-4, US1-5, US2-1) — buffering install holding ALL
  sink-bound frames (PtyOutput, TrimScrollback, ScrollBottom) in
  emission order per the committed-byte cursor design; bounded
  per-sink writer queues; Q6 overflow/resync policy. Out of scope:
  the #60/#61 `Arc`/pre-framed payload change. Revert criterion:
  e2e-visual regression or output-ordering corruption in daemon.rs
  assertions ⇒ restore install-after-replay order behind a flag.
  Blocked by "scribe-test SessionReplay support" and "Baseline:
  search and attach lock/alloc measurements". Blocks "fresh-session
  create skips replay" (its ED2 variant) and Wave 2 "wire
  drain_until_frame pacing" (#64's safety net).
- **Streamed replay decompression with 64 MiB ceiling** (#4; US2-2)
  — replaces the 8 B/cell bound; handoff of truecolor-dense screens
  no longer blanks.
- **Attach fan-out dedup, cap, and off-thread encode** (#17, #18;
  US2-3) — dedup by session id, 8 concurrent replay builds,
  snapshot+zstd via `spawn_blocking`. P5: closes the LAN-reachable
  uncapped spawn.
- **Client replay inflation off the current-thread runtime** (#19;
  US2-3).
- **Fresh-session create skips replay and uses real geometry** (#22,
  #23; US2-1, US2-4) — no redundant attach/replay, no 120×36
  shrink/regrow, at most one SIGWINCH; daemon.rs harness update.
  Blocked by "per-session sink state machine" and "scribe-test
  SessionReplay support".

US4 correctness (the shell-script items serialize — B8: "helper
resolution and packaging" blocks "per-shell restore-file rendering"
blocks "helper payload transport off argv"; all Wave 2 shell-hook
items depend on all three):

- **Helper resolution and packaging** (#26; US4-1) —
  `SCRIBE_HOOK_HELPER` injected var (spec OQ5 recommendation,
  consistent with acceptance criterion US4-1), resolved in prod-deb,
  dev-deb, DMG, dev-build layouts; deb ships nushell/pwsh scripts;
  var joins the exclusion set. Blocked by "Baseline: per-prompt shell
  hook cost". Blocks "per-shell restore-file rendering" and Wave 2
  adapter work.
- **Per-shell restore-file rendering** (#27, #28, #29; US4-2) —
  ShellKind-driven rendering + extension (`.ps1` for pwsh), nu-safe
  encoding, shell detection moved before `prepare_restore_env_file`.
  Blocked by "helper resolution and packaging". Blocks "helper
  payload transport off argv".
- **Envelope id minting, bootstrap, and orphan GC** (#30, #31;
  US4-3) — all three create paths mint ids (Q4b); first-envelope
  bootstrap without restart; startup-scan GC deletes orphaned
  envelopes older than 30 days.
- **Enable/disable persistence semantics** (#32; US4-4) — restated:
  server-side disable is already live (`ipc_server.rs:6993-6998` —
  timers stop, envelopes deleted; existing behavior preserved and
  verified unchanged); the shell-side snapshot/diff gate is
  spawn-time (new shells only — that is the #33/#34 fix); enable
  establishes a baseline in the first newly started shell
  (`hook_ingress.rs:188-195`), per Q4a. Code-level acceptance: after
  enable with no server restart, the next new shell's baseline
  creates the envelope and persistence works; after disable, persist
  writes stop immediately while running shells' hook behavior is
  unchanged; the semantic is documented in lat.md.

Admission, availability, and security:

- **Session cap counts live sessions atomically** (#1, #2 — land
  together; US7-1) — the 256-session `MAX_SESSIONS` cap counts live
  sessions via a permit/semaphore like `MAX_CONNECTIONS`; counts
  handoff-restored sessions; typed error at cap.
- **Transient hook connection slots and pre-Hello timeout** (#46;
  US5-5) — moved from Wave 2: an availability defect (hook bursts
  can exhaust the 32 long-lived client slots). Separate 16-slot
  semaphore; 5 s pre-Hello timeout on local connections (Q7).
- **Helper payload transport off argv** (#42, #43, #44, #45; US5-1,
  US5-2) — stdin / 0600 temp file (env-apply model), dual-accept one
  release, E2BIG silent-loss eliminated, prompt no longer in
  `/proc/*/cmdline`. P5 priority. Blocked by "per-shell restore-file
  rendering" (script serialization), "Baseline: hook pipeline exec
  counts", and "Baseline: per-prompt shell hook cost". Blocks Wave 2
  "Codex adapter consolidation".

US9 installer:

- **Installer atomic writes** (#79; US9-1) — tmp + `os.replace` for
  the step-2 bare `write_text` (and any sibling gaps in
  setup-claude-hooks.sh).
- **Installer single read-modify-write, idempotent** (#78 [80
  folded]; US9-2) — one RMW per file per run;
  equality-check-before-write keeps mtime/inode stable.

US8 search lock fix:

- **Search scan outside the Term lock** (#70; US8-1) — snapshot-only
  under lock; scan lock-free. Blocked by "Baseline: search and attach
  lock/alloc measurements".
- **Search debounce and snapshot reuse** (#69; US8-2) — 150 ms client
  debounce; one snapshot per overlay-open, invalidated on new output
  (Q5 option b). Blocked by "search scan outside the Term lock".

### Wave 2 — hot-path perf polish

US3 client pipeline ordering: "batch byte cap" lands first in the
drain; "off-lock VTE parse and O(n) splitter" (#68) **blocks** "wire
drain_until_frame pacing" (#62) — `drain_all_committed` sits inside
the queue+`panes` lock scope that #68 dismantles, so the pacing
wire-in lands on the restructured drain (kept as two work items with
this explicit edge); pacing in turn blocks "skip invisible
intermediate-frame rebuilds" (#64).

- **Bounded inbound queue with coalesce/drop-and-resync** (#14;
  US3-1) — 256 events; self-detected overflow → `RequestSnapshot`,
  per participant (Q6/Q7).
- **Bounded outbound queue, tear-on-cap** (#15; US3-1) — 1024 frames;
  never drop input per-frame; cap ⇒ tear connection / refuse input
  with visible feedback (Q6). OQ7 is decided here: **no pruning** of
  queued non-attach-gated frames on redial — a queued `CreateSession`
  firing after reconnect reflects real user intent and matches the
  #16 "no independent action" disposition. The criterion is purely
  the bounded queue + tear-on-cap policy.
- **Batch byte cap** (#65 [66 folded]; US3-2) — 1 MiB per batch;
  oversize events processed alone. Lands before "off-lock VTE parse
  and O(n) splitter" (same drain).
- **Off-lock VTE parse and O(n) splitter** (#68 [67 folded]; US3-2,
  US3-4) — parse outside the `panes`+sync mutex scope the render
  thread needs; splitter drops per-byte `remove(0)`. Blocks "wire
  drain_until_frame pacing".
- **Wire drain_until_frame pacing** (#62 [63 folded]; US3-3) —
  restore one-burst-per-redraw; define `SessionReplay` as its own
  burst boundary; reconcile `lat.md/client.md:1052`;
  `lat.md/test.md` drain_until_frame test-spec entries remain valid
  (Q3). Blocked by "off-lock VTE parse and O(n) splitter", Wave 1
  "per-session sink state machine", and "scribe-test SessionReplay
  support". Blocks "skip invisible intermediate-frame rebuilds".
- **Skip invisible intermediate-frame rebuilds** (#64; US3-3) — no
  `make_content` under the `panes` mutex for frames the pacer skips.

US5 hook pipeline (#46 moved to Wave 1 as an availability defect):

- **Codex adapter consolidation** (#37, #38 [40 folded]; US5-3) — one
  interpreter run per event (or Python eliminated), transcript-tail
  memoization, single adapter per Stop/PostToolUse; includes installer
  trusted-hash migration + acceptance for already-registered hooks;
  before/after exec counts vs Wave 0 baseline (#36/#39 may fall out
  free). Blocked by Wave 1 "helper payload transport off argv" and
  "Baseline: hook pipeline exec counts".
- **AiStateChanged equality guard** (#41; US5-4) — suppress unchanged
  percentages before broadcast.

US6 metadata:

- **Single-source TitleChanged** (#48 [50 folded]; US6-1) — one
  emitter; `;`-containing titles produce one title; halves registry
  write-locks/frames.
- **Single-source Bell** (#49 [51 folded]; US6-1) — one Bell per BEL;
  single `request_attention`.
- **OSC 7 last-value suppression** (#52; US6-2) — dedup before
  registry/branch/workspace work. Blocks "git-branch cache" (defines
  the invalidation signal).
- **Git-branch cache and detached-session skip** (#54, #55; US6-2) —
  `(session, cwd)` cache, OSC 7 invalidation + 5 s TTL; skip
  detection when sink set empty.
- **ListSessions branch resolution off registry guards** (#56, #57;
  US6-3) — resolve after dropping `live_sessions` +
  `workspace_manager` read guards; share per-(cwd) results across
  panes. Blocked by "git-branch cache and detached-session skip".
- **Empty-frame send guard** (#73 [74 folded]; US6-4) — `is_empty`
  check after ED3/picker filters in `apply_pty_filters`/send path
  (not OSC); daemon.rs harness update.
- **Config/theme cache for dynamic color queries** (#75 [76 folded];
  US6-5) — cache parsed config + resolved theme, invalidate on
  reload; OSC 4 probe = zero disk reads warm.
- **Launch-path caching in packaged builds only** (#82 [81 folded];
  US6) — **ratified at the analyze gate (2026-07-29)** — spec OQ6, recorded as
  Clarification Q8; approved before cutting the
  bead. Memoize `find_scripts_dir` (+ fold the `detect_shell`
  cleanup) only when running from a packaged layout, preserving
  dev-build hot-swap of integration scripts. Small item; may be
  dropped without impact.

Shell-hook waste (US4 perf half; each item is blocked by all three
Wave 1 shell-script items — "helper resolution and packaging",
"per-shell restore-file rendering", "helper payload transport off
argv" — and by "Baseline: per-prompt shell hook cost"):

- **Persistence gate var and disabled-path skip** (#33, #34; US4-5) —
  spawn-time gate var; shells skip baseline snapshot, helper fork, and
  per-prompt snapshot/diff while disabled; gate var joins the
  exclusion set (Q4c).
- **Fish/nu linear-time env diffs** (#35; US4-6) — replace O(N²) list
  scans.
- **Nu XDG_DATA_DIRS cleanup** (#72; US4-6) — strip injected value
  before children/baseline.

US7 polish (verified per the US7 scripted resize/viewport scenario):

- **Viewport-report trailing debounce** (#24; US7-3) — cancel/
  generation-check the 250 ms timer; one trailing apply.
- **Resize coalescing to ≤4 applies/sec** (#25; US7-3) — continuous
  drag settles instead of full reflow per event.

## Traceability

Mechanically checkable coverage: every one of the 82 findings maps to
exactly one work item, fold target, or explicit non-goal. The
`US<n>-<m>` ids below are stable through bead creation and are the
ids referenced by the Sequencing items.

### Acceptance-criterion ids

- **US1-1** close/window-close never blocks locks or Tokio workers;
  **US1-2** real exit codes/signals via child watcher; **US1-3**
  readers cancellable, joinable, bounded, no hot-spin; **US1-4**
  stalled client cannot back-pressure reader/Term, no mutex held
  across sink awaits; **US1-5** SIGSTOP'd-client + `yes` scenario
  passes.
- **US2-1** no snapshot/install gap, incl. the ED2 re-point variant;
  **US2-2** decompression bound from encoded size, 64 MiB streamed;
  **US2-3** fan-out dedup/cap + off-thread encode + client inflate
  offload; **US2-4** fresh sessions skip replay, real geometry, at
  most one SIGWINCH.
- **US3-1** bounded queues with explicit overflow policy; **US3-2**
  byte-bounded batches, parse off the `panes` lock; **US3-3** paced
  one-burst-per-redraw incl. `SessionReplay` ordering, no invisible
  rebuilds; **US3-4** splitter drops per-byte `remove(0)`.
- **US4-1** packaged helper resolution (four layouts); **US4-2**
  per-shell restore files; **US4-3** envelope id on all three create
  paths + bootstrap + GC; **US4-4** enable/disable semantics correct
  and documented; **US4-5** disabled shells skip snapshot/diff;
  **US4-6** fish/nu O(N) diffs and no `XDG_DATA_DIRS` leak.
- **US5-1** no secrets in argv; **US5-2** no silent E2BIG loss;
  **US5-3** one interpreter run per Codex event, measured exec drop;
  **US5-4** `AiStateChanged` equality guard; **US5-5** separate
  transient hook slots + pre-Hello timeout.
- **US6-1** one TitleChanged and one Bell per sequence; **US6-2**
  OSC 7 suppression + branch cache + detached skip; **US6-3**
  ListSessions branch work off registry guards; **US6-4** no empty
  PtyOutput frames; **US6-5** cached config/theme for color queries.
- **US7-1** 256 cap counts live sessions atomically; **US7-2** PID
  identity before SIGHUP; **US7-3** viewport debounce + resize ≤4
  applies/sec.
- **US8-1** scan outside the Term lock; **US8-2** debounce + snapshot
  reuse.
- **US9-1** atomic tmp+rename installer writes; **US9-2** single
  idempotent RMW per file.

### Finding → disposition map

| # | Disposition |
|---|-------------|
| 1 | "Session cap counts live sessions atomically" (US7-1) |
| 2 | "Session cap counts live sessions atomically" (US7-1) |
| 3 | "Per-session sink state machine" (US2-1) |
| 4 | "Streamed replay decompression with 64 MiB ceiling" (US2-2) |
| 5 | "Pty teardown off-worker via PtyGuard" (US1-1) |
| 6 | "Close-path lock protocol and bounded reader join" (US1-1) |
| 7 | "Reader cancellation and retained handles" (US1-3) |
| 8 | "Reader cancellation and retained handles" (US1-3) |
| 9 | "Close-path lock protocol and bounded reader join" (US1-3) |
| 10 | "Handoff PID-identity check before SIGHUP" (US7-2) |
| 11 | "Child-exit watcher with real exit codes" (US1-2) |
| 12 | "Orphaned-reader hot-spin and descendant-held-slave exit" (US1-3) |
| 13 | "Child-exit watcher with real exit codes" (US1-2) |
| 14 | "Bounded inbound queue with coalesce/drop-and-resync" (US3-1) |
| 15 | "Bounded outbound queue, tear-on-cap" (US3-1) |
| 16 | Non-goal — see "Not fixed" below |
| 17 | "Attach fan-out dedup, cap, and off-thread encode" (US2-3) |
| 18 | "Attach fan-out dedup, cap, and off-thread encode" (US2-3) |
| 19 | "Client replay inflation off the current-thread runtime" (US2-3) |
| 20 | Fold → #21 ("Per-session sink state machine") |
| 21 | "Per-session sink state machine" (US1-4) |
| 22 | "Fresh-session create skips replay and uses real geometry" (US2-1, US2-4) |
| 23 | "Fresh-session create skips replay and uses real geometry" (US2-4) |
| 24 | "Viewport-report trailing debounce" (US7-3) |
| 25 | "Resize coalescing to ≤4 applies/sec" (US7-3) |
| 26 | "Helper resolution and packaging" (US4-1) |
| 27 | "Per-shell restore-file rendering" (US4-2) |
| 28 | "Per-shell restore-file rendering" (US4-2) |
| 29 | "Per-shell restore-file rendering" (US4-2) |
| 30 | "Envelope id minting, bootstrap, and orphan GC" (US4-3) |
| 31 | "Envelope id minting, bootstrap, and orphan GC" (US4-3) |
| 32 | "Enable/disable persistence semantics" (US4-4) |
| 33 | "Persistence gate var and disabled-path skip" (US4-5) |
| 34 | "Persistence gate var and disabled-path skip" (US4-5) |
| 35 | "Fish/nu linear-time env diffs" (US4-6) |
| 36 | Non-goal — see "Not fixed" below |
| 37 | "Codex adapter consolidation" (US5-3) |
| 38 | "Codex adapter consolidation" (US5-3) |
| 39 | Non-goal — see "Not fixed" below |
| 40 | Fold → #38 ("Codex adapter consolidation") |
| 41 | "AiStateChanged equality guard" (US5-4) |
| 42 | "Helper payload transport off argv" (US5-1) |
| 43 | "Helper payload transport off argv" (US5-1) |
| 44 | "Helper payload transport off argv" (US5-1) |
| 45 | "Helper payload transport off argv" (US5-2) |
| 46 | "Transient hook connection slots and pre-Hello timeout" (US5-5; Wave 1) |
| 47 | Non-goal — see "Not fixed" below |
| 48 | "Single-source TitleChanged" (US6-1) |
| 49 | "Single-source Bell" (US6-1) |
| 50 | Fold → #48 ("Single-source TitleChanged") |
| 51 | Fold → #49 ("Single-source Bell") |
| 52 | "OSC 7 last-value suppression" (US6-2) |
| 53 | Non-goal — see "Not fixed" below |
| 54 | "Git-branch cache and detached-session skip" (US6-2) |
| 55 | "Git-branch cache and detached-session skip" (US6-2) |
| 56 | "ListSessions branch resolution off registry guards" (US6-3) |
| 57 | "ListSessions branch resolution off registry guards" (US6-3) |
| 58 | "Per-session sink state machine" (US1-4) |
| 59 | Fold → #58/#21 ("Per-session sink state machine") |
| 60 | Non-goal (deferred) — see "Not fixed" below |
| 61 | Non-goal (deferred) — see "Not fixed" below |
| 62 | "Wire drain_until_frame pacing" (US3-3) |
| 63 | Fold → #62 ("Wire drain_until_frame pacing") |
| 64 | "Skip invisible intermediate-frame rebuilds" (US3-3) |
| 65 | "Batch byte cap" (US3-2) |
| 66 | Fold → #65 ("Batch byte cap") |
| 67 | Fold → #68 ("Off-lock VTE parse and O(n) splitter") |
| 68 | "Off-lock VTE parse and O(n) splitter" (US3-2, US3-4) |
| 69 | "Search debounce and snapshot reuse" (US8-2) |
| 70 | "Search scan outside the Term lock" (US8-1) |
| 71 | Non-goal (INVALID) — see "Not fixed" below |
| 72 | "Nu XDG_DATA_DIRS cleanup" (US4-6) |
| 73 | "Empty-frame send guard" (US6-4) |
| 74 | Fold → #73 ("Empty-frame send guard") |
| 75 | "Config/theme cache for dynamic color queries" (US6-5) |
| 76 | Fold → #75 ("Config/theme cache for dynamic color queries") |
| 77 | Non-goal — see "Not fixed" below |
| 78 | "Installer single read-modify-write, idempotent" (US9-2) |
| 79 | "Installer atomic writes" (US9-1) |
| 80 | Fold → #78/#79 (installer items) |
| 81 | Fold → #82 ("Launch-path caching in packaged builds only") |
| 82 | Optional: "Launch-path caching in packaged builds only" (US6; ratified at analyze gate) |

### Not fixed (with rationale)

Mirrors the spec's Non-Goals; bead authors must not re-litigate these:

- **#16** — retention harm is gated off by the fresh `attached_ids`
  check on the new connection; retention itself is covered by #15's
  bounded queue. The offline-`CreateSession` wrinkle is decided
  (OQ7): no pruning on redial.
- **#20** — folded into #21: same inline-await defect, one fix.
- **#36** — two SessionStart helper runs are a one-shot cost; may
  fall out of the #37/#38 consolidation for free.
- **#39** — per-invocation Tokio runtime build is µs against ms exec
  cost.
- **#47** — AI-facing silence is the specified FR-007/008/009 safety
  contract; server-side drops already log `warn!`.
- **#53** — `/proc` readlink is result-deduped and cheap;
  `spawn_blocking` would cost more than it saves.
- **#60, #61** — per-participant copies/serialization are real but
  bounded; the `Arc`/pre-framed payload protocol change is
  disproportionate to current participant counts. Deferred and
  explicitly out of scope of the sink state machine.
- **#67 (as stated)** — the cited `extend_from_slice` memcpy is the
  cheapest copy on the path; the real per-byte splitter cost is
  fixed under #68.
- **#71** — INVALID: bash scalar assignment to array
  `PROMPT_COMMAND` writes index 0 and preserves elements 1..n
  (verified on bash 5.2.21).
- **#77** — two install-time heredoc interpreters, one-shot cost.
- **#80** — folded into #78/#79 as equality-check-before-write, not
  a separate bead.
- **#81** — `detect_shell` is a `file_stem` string match,
  unmeasurable; folded into #82's launch caching if that lands, else
  dropped.

## Backlog Refinement

None — no backlog inputs; no source P4 issues exist. (Spec's Backlog
Inputs section confirms: no `epic`, `source_backlog`, or `backlog`
variables, and no related open P4 sources in the tracker.)

## Target Epic

A new epic will be created at the create-beads step.

## Constitution Check

- **P1 (Clear Boundaries and Typed Failure)** — PASS. `PtyGuard`, the
  sink state machine, and caches live in scribe-server; client queue/
  pacing work stays in scribe-client; helper transport in
  scribe-hook-helper. alacritty is wrapped, not forked. Cap overflow,
  queue tear-down, and skip-signal paths use typed errors; no
  cross-cutting helper sprawl (the only new shared surface is two env
  var names).
- **P2 (Session-Safe, Consistent UX)** — PASS with a resolved tension:
  bounded queues could drop user-visible state. Resolved by Q6 —
  output overflow coalesces then resyncs via `RequestSnapshot`
  (continuity strengthened, never silently wrong); input is never
  dropped per-frame (tear + visible feedback). Reattach/handoff
  guarantees are preserved or strengthened (#3 losslessness, #4
  no-blank-handoff); inherited-session exemptions are documented, not
  silent.
- **P3 (Explicit, Risk-Based Verification)** — PASS. Every US names a
  user-reachable path (dev-server identity, `just e2e-func`, manual
  SIGSTOP/`yes`/trap-HUP scenarios). Test code changes limited to
  existing-coverage collateral (daemon.rs, sync_frames.rs, lat.md test
  specs).
- **P4 (Performance Budgets and Measurement)** — PASS. Wave 0 captures
  exec counts, lock-hold/alloc, and per-prompt counts at `b90c932`
  before any fix merges (unrecoverable otherwise); US5/US6/US8/US3
  acceptance states before/after numbers; Q7 fixes the numeric
  budgets.
- **P5 (Default-Safe Trust Boundaries)** — PASS with a resolved
  tension: the #4 ceiling raise is on a LAN-reachable untrusted path;
  resolved by Q7's streamed decode + 64 MiB absolute post-inflate cap,
  never peer-declared size. #44 removes secrets from world-readable
  `/proc/*/cmdline` (0600 transport); #10 gates SIGHUP on PID
  identity; #17 caps/dedups LAN-triggerable replay builds.
- **P6 (Local-First Data Locality)** — PASS. No new network surface;
  all fixes are local-path; nothing transmits terminal contents beyond
  existing attach semantics.
- **P7 (Compatible, Documented, Operationally Safe Change)** — PASS
  with a resolved tension: server-side fixes normally want a restart
  to verify; resolved by verifying exclusively on the
  `scribe-dev-server` identity + docker e2e — the live server is
  never restarted. All wire/handoff changes additive at
  HANDOFF_VERSION 6 with N/N-1 acceptance; helper dual-transport
  covers in-memory old shells; `lat.md` is reconciled (client.md:1052
  pacing prose, test.md pacing specs stay valid per Q3) as part of the
  relevant beads, and compatibility decisions (inherited-session
  exemptions, dual-accept window) are documented in spec + lat.md.

## Alignment fixes applied

- [A1, must] Added "## Traceability": full 82-finding → disposition
  table plus stable `US<n>-<m>` acceptance-criterion ids, now
  referenced by every Sequencing work item.
- [A2, must] Added "Not fixed (with rationale)" mirroring the spec's
  Non-Goals (#16, #20, #36, #39, #47, #53, #60, #61, #67, #71, #77,
  #80, #81) so bead authors don't re-litigate them.
- [B1, must] Expanded lat.md sync scope with a per-work-item target
  table (server.md PTY Reader Task / Detach and Reattach / Handoff
  Session Replay Encoding + Size Limits + Defuse Strategy / Env
  Persistence Runtime Enable-Disable Transitions / Shell Integration
  / Git Branch Detection; protocol.md Session Events + SessionReplay;
  pty.md Metadata Parser); `lat check` gates every bead.
- [B2, must] Fixed the sink state machine design hole: Buffering now
  holds ALL sink-bound frames (incl. TrimScrollback/ScrollBottom,
  `ipc_server.rs:7443-7457`) in emission order, offset-tagged, so a
  buffered attach cannot apply a trim/bottom-snap against the wrong
  snapshot.
- [B3, must] Added the explicit edge "off-lock VTE parse (#68) blocks
  wire drain_until_frame pacing (#62)"; kept as two work items with
  the dependency stated.
- [B4, must] Corrected #32 semantics: server-side disable is already
  live (behavior preserved); shell-side gate is spawn-time (the
  #33/#34 fix); enable needs a new shell for baseline. Item now has a
  code-level acceptance criterion.
- [B5, must] Added Wave 0 "Baseline: per-prompt shell hook cost"
  (enabled AND disabled, per shell), blocking the Wave 1 script
  rewrites and Wave 2 shell-hook-waste items.
- [B6, must] Promoted "scribe-test SessionReplay support"
  (`daemon.rs:383-387`) to an early Wave 1 item; #3, #22, and #62
  now depend on it.
- [A4/B7, should] Corrected counts: 60 Fix=yes + optional #82 = 61
  findings; final work-item count 48 (Wave 0: 4, Wave 1: 23,
  Wave 2: 21).
- [A3, should] #82 marked "unratified recommendation — ratify at the
  analyze gate" and attached to US6.
- [A5, should] Out-of-scope markers for the #60/#61 `Arc`/pre-framed
  payload change on the framing.rs component bullet and the sink
  state machine item.
- [A6, should] `SCRIBE_HOOK_HELPER` restated as a spec OQ5
  recommendation consistent with US4-1, not "approved".
- [A7, should] Added the numeric 256 `MAX_SESSIONS` cap to the #1+#2
  work item.
- [B8, should] Serialized the shell-script items: #26 → #27-29 →
  #42-45; Wave 2 shell-hook items depend on all three.
- [B9, should] Decided OQ7 in the plan: no pruning of queued
  non-attach-gated frames on redial; #15's criterion is purely the
  bounded queue + tear-on-cap policy.
- [B10, should] Envelope GC retention fixed: orphaned envelopes (no
  live window/launch reference) older than 30 days deleted at
  server-startup scan.
- [B11, should] Wave 0 baselines name the work items they block and
  record results to `specs/017-audit-findings-triage/baselines.md`.
- [B12, should] Moved #46 (transient hook slots + pre-Hello timeout)
  from Wave 2 to Wave 1 as an availability defect.
- [B13, should] Added revert criteria: sink state machine (flagged
  install-after-replay fallback on e2e-visual or ordering
  regression) and the additive HandoffState field (drop field, serde
  default, on failed prior-version decode in dev-identity
  rehearsal).
- [B14, should] Added US7 verification: scripted resize-drag /
  viewport-report scenario on the dev identity; apply rate settles
  to ≤4 applies/sec.
