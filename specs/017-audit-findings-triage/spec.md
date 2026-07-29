# Spec: audit-findings-triage

## Problem Statement

An 82-item audit of Scribe surfaced suspected defects and inefficiencies
across session/PTY lifecycle, IPC queues and coalescing, attach/replay,
AI hooks and helper processes, shell integration, environment
persistence, OSC/metadata handling, shared-session fanout, search,
client rendering, and installers. Each finding has now been validated
against the codebase at `b90c932` by parallel code investigation. 81 of
82 findings are VALID or PARTIALLY-VALID; 1 is INVALID (#71); several
are valid but judged not worth independent fixes. This feature turns the
validated findings into implementation-ready task beads so the defects
can be fixed systematically, and explicitly dispositions the rest as
non-goals.

## Goals

- Every VALID finding worth fixing is covered by an implementation-ready
  task bead with concrete file/line anchors and acceptance criteria.
- Correctness/data-loss defects (attach output loss, replay decompression
  ceiling, env-persistence bootstrap, fish/pwsh/nushell restore, packaged
  helper resolution) are fixed so the affected features actually work.
- Availability defects (blocking `Pty::drop` under the global lock,
  stalled-local-socket PTY back-pressure, search under the Term lock) no
  longer allow one session or one client to stall the server or window.
- Security exposures (#44 secrets in argv, #10 PID reuse, #17 unbounded
  attach fan-out from LAN peers) are closed with default-safe behavior
  (constitution P5).
- Hot-path waste (duplicate OSC events, per-prompt branch detection,
  per-query config reload, per-prompt env work while disabled) is
  eliminated or gated, with measurable before/after checks where
  practical (constitution P4).
- Findings judged invalid or not-worth-fixing are recorded as explicit
  non-goals with rationale — nothing is silently dropped.

## Non-Goals

Dispositioned after investigation (evidence in Findings Disposition
below); each requires human confirmation at the clarify gate:

- **#71 (INVALID)** — bash scalar assignment to array `PROMPT_COMMAND`
  writes index 0 and preserves elements 1..n (verified on bash 5.2.21);
  no handlers are lost. No action.
- **#16 (harm claim wrong)** — stale resize/search/focus retained across
  reconnect are all rejected by the per-connection `attached_ids` gate on
  the new connection; retention itself is covered by #15. No independent
  action (the un-gated `CreateSession`-while-offline wrinkle is noted as
  an open question).
- **#47 (by design)** — silent hook failure toward the AI tool is the
  specified FR-007/008/009 safety contract; server-side drops already
  log `warn!`. At most an opt-in debug-log follow-up; not in scope.
- **#20** — folded into #21 (same defect, one fix).
- **#36, #39, #77** — real but one-shot/µs-scale costs (two SessionStart
  helper events, per-invocation Tokio runtime build, installer starting
  Python twice); not worth independent changes. #36/#39 may fall out of
  the helper/adapter consolidation in #37/#38 for free.
- **#53** — `/proc` readlink on the reader is result-deduped and cheap;
  `spawn_blocking` would cost more than it saves. No action.
- **#60, #61** — per-participant copies/serialization are real but
  bounded; fixing needs an `Arc`/pre-framed payload protocol change
  disproportionate to current participant counts. Deferred.
- **#67 (as stated)** — the cited `extend_from_slice` memcpy is the
  cheapest copy on the path; the genuinely expensive per-byte
  `SyncUpdateFrameSplitter` cost is addressed under #68's task instead.
- **#80** — installer mtime churn is cosmetic; folded into the #78/#79
  installer task as an equality-check-before-write, not a separate bead.
- **#81** — `detect_shell` is a `file_stem` + string match; unmeasurable.
  Folded into #82's launch-caching cleanup if that lands, else dropped.
- Rewrites beyond the findings (no supervisor-process redesign, no wire
  protocol v6, no replacing alacritty_terminal).

## Backlog Inputs

None. No `epic`, `source_backlog`, or `backlog` variables were provided,
and the tracker contains no open P4 sources related to these findings.

## Target Epic

No existing epic was provided or inferred. This run will create a new
feature epic for the audit-findings fixes during `create-beads`.

## Findings Disposition

Verdicts from parallel code investigation (all line refs at `b90c932`).
Severity in parentheses; "fix" = gets a task bead.

### A. Session lifecycle (crates/scribe-server)

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 1 | VALID | med | yes | `MAX_SESSIONS` checks staging map only; cap is dead code (`session_manager.rs:356-364`, `ipc_server.rs:5532`) |
| 2 | VALID (latent until #1 fixed) | low | yes | read-guard check, insert after awaits (`session_manager.rs:342-352`) |
| 5 | VALID | high | yes | alacritty `Pty::drop` does blocking `waitpid`; dropped from async tasks (`ipc_server.rs:5881,5928-5931,7655`) |
| 6 | VALID | high | yes | `handle_close_window` drops Pty under global `live_sessions` write guard (`ipc_server.rs:5926-5932`) |
| 7 | VALID | low | yes | reader `JoinHandle` discarded (`ipc_server.rs:5693`) |
| 8 | VALID | med | yes | no cancellation token/stop variant in `PtyReaderState` (`ipc_server.rs:741-815,7320-7355`) |
| 9 | VALID | low-med | yes | teardown never joins/bounds reader exit (`ipc_server.rs:5858-5957`, `main.rs:252-268`) |
| 10 | VALID | med | yes | handoff SIGHUP to raw wire PID, no identity check (`ipc_server.rs:5835-5844`, `handoff.rs:141`) |
| 11 | VALID | high | yes | no waitpid/child watcher anywhere; exit = master EOF only (`ipc_server.rs:7677`) |
| 12 | VALID | med-high | yes | descendants keep slave open → reader lives; orphaned reader hot-spins on closed clipboard channel (`ipc_server.rs:7344-7400`) |
| 13 | VALID | med | yes | `SessionExited { exit_code: None }` hardcoded (`ipc_server.rs:7652`) |

### B. Attach/replay and client queues

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 3 | VALID | high | yes | replay built+sent before sink install; sink-less sends are no-ops (`attach_flow.rs:151-215`, `ipc_server.rs:7299-7305`) |
| 4 | VALID | high | yes | zstd decompress capped 8 B/cell; encoder can emit 50+ B/cell; handoff fallback is blank session (`screen_replay.rs:69-72,276-337`) |
| 14 | VALID | med | yes | inbound `unbounded_channel`, no backpressure (`client main.rs:6502,8983-8996`) |
| 15 | VALID | med | yes | outbound unbounded, accumulates across infinite redials (`main.rs:6501,6731-6785`) |
| 16 | PARTIAL | low | no | retention real; harm gated off by fresh `attached_ids` (`ipc_server.rs:3600,6086-6126`) |
| 17 | VALID | med | yes | one uncapped spawn per attach entry, no dedup; LAN-reachable (`attach_flow.rs:118-135`) |
| 18 | VALID | med | yes | snapshot+zstd inline on async task, no `spawn_blocking` (`attach_flow.rs:268-275`) |
| 19 | VALID | med | yes | replay inflated inline on client current-thread runtime (`main.rs:7880-7881`, `session_lifecycle.rs:63-73`) |
| 22 | VALID | med | yes | fresh session re-runs full attach/replay; replay ED2 can erase startup bytes (`ipc_server.rs:5538-5623`, `main.rs:8886-8975`) |
| 23 | VALID | med | yes | reader-side attach uses fixed 120×36 startup grid → shrink/regrow + extra SIGWINCH (`main.rs:8963-8971`, `ipc_server.rs:6096-6114`) |

### C. PTY output path, resize, empty frames

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 20 | VALID | med | fold→21 | inline awaited local socket write in reader loop (`ipc_server.rs:7447`, `framing.rs:71`) |
| 21 | VALID | high | yes | stalled local socket parks reader → PTY buffer fills → child blocks; also freezes Term + sink mutex (`ipc_server.rs:1237-1250,7300-7304,7462`) |
| 24 | VALID | low | yes | one uncancelled 250 ms timer per viewport report (shared modes only) (`ipc_server.rs:6017-6021`) |
| 25 | VALID | med | yes | continuous change → applies at event rate, full reflow per pane per step (`ipc_server.rs:6039-6076`) |
| 73 | PARTIAL (attribution wrong: ED3/picker filters, not OSC) | low | yes | no `is_empty` guard on send (`ipc_server.rs:7436-7447`, `ed3_filter.rs:64-71`) |
| 74 | VALID | low | fold→73 | empty frames alloc/serialize/queue/parse/coalesce end-to-end (`ipc_server.rs:7685`, `ipc_bridge.rs:140-174`) |

### D. Shell integration and env persistence

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 26 | VALID | high | yes | scripts use bare PATH lookup; packages install helper off-PATH; nushell/pwsh scripts missing from deb entirely (`dist/shell-integration/*`, `scribe-server/Cargo.toml:63-67,93-113`) |
| 27 | VALID | high | yes | restore file is POSIX-only; fish has no `export`/`unset` → restore fully broken (`session_manager.rs:1209-1221`, `scribe.fish:142-149`) |
| 28 | VALID | med-high | yes | pwsh dot-sources a `.sh` POSIX file → silent no-op (`scribe.ps1:135-154`) |
| 29 | VALID | med | yes | nu applier mishandles `'\''` and multi-line values; substring ranges likely off-by-one (`scribe.nu:205-241`) |
| 30 | VALID | high | yes | GUI/CLI/test create paths hardcode `env_envelope_id: None` (`ipc_bridge.rs:334-341`) |
| 31 | VALID | high | yes | no bootstrap: persist skipped without envelope id ("T016 compromise", `hook_ingress.rs:240-256`) |
| 32 | VALID | med | yes | enable-while-running establishes no baseline; baseline emitted once at shell init only (`ipc_server.rs:6993-6998`, `hook_ingress.rs:188-195`) |
| 33 | VALID | low-med | yes | no shell-visible gate; baseline snapshot+helper fork on every shell start while disabled (`scribe.bash:356-366` et al.) |
| 34 | VALID | med | yes | full snapshot+diff every prompt while disabled (`scribe.bash:328-352` et al.) |
| 35 | VALID | low-med | yes | fish (and nu) diff is O(N²) list scans per prompt (`scribe.fish:247,270`, `scribe.nu:266,272`) |
| 71 | INVALID | — | no | bash array `PROMPT_COMMAND` preserved; verified on bash 5.2.21 (`scribe.bash:137-141,371-377`) |
| 72 | VALID | low-med | yes | nu never strips injected `XDG_DATA_DIRS`; leaks to children and into persisted baseline (`shell_integration.rs:132-141`, no cleanup in `scribe.nu`) |

### E. AI hooks, helper, installer

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 36 | VALID | low | no | two sequential SessionStart helper runs; needs wire change to merge (`ai-hook-codex.sh:52-62`) |
| 37 | VALID | med | yes | ~8-12 execs per Codex event; Stop/PostToolUse register two adapters each (`ai-hook-codex.sh:65-119`, `setup-codex-hooks.sh:201-210`) |
| 38 | VALID | med | yes | same JSON parsed by 2-3 separate python3 per event (`ai-hook-codex.sh:36-47,69-113`) |
| 39 | VALID | low | no | per-invocation runtime build is µs vs ms exec cost (`hook-helper main.rs:121-126`) |
| 40 | VALID | low-med | fold→38 | 64 KiB transcript tail re-read/re-parsed per tool call, no memo (`ai-hook-codex.sh:199-232`) |
| 41 | VALID | med | yes | no equality guard before `AiStateChanged` broadcast (`ipc_server.rs:8675-8698`) |
| 42 | VALID | med | yes | full prompt in argv, then truncated to 256 B server-side (`ai-hook-*.sh:83-91`, `hook_ingress.rs:302`) |
| 43 | VALID | med | yes | full env baseline/delta JSON in one argv string (`scribe.bash:343-363`) |
| 44 | VALID (understated) | high | yes | `/proc/*/cmdline` is world-readable → secrets widened beyond owner (`scribe.bash:361-362`, `delta.rs:32-67`) |
| 45 | VALID | med | yes | 128 KiB `MAX_ARG_STRLEN` vs 512 KiB server cap; E2BIG exits 0 silently (`scribe.bash:345`, `delta.rs:15,19`) |
| 46 | VALID | med | yes | transient hooks share the 32-slot semaphore; overflow drops with no Busy; no pre-Hello timeout on local (`ipc_server.rs:1051-1063,3789-3799`) |
| 47 | PARTIAL (by design) | low | no | AI-facing silence is the specified contract; only helper-side budget expiry is unobservable (`hook-helper main.rs:89-125`) |
| 77 | VALID | low | no | two heredoc interpreters, install-time once (`setup-codex-hooks.sh:57,112`) |
| 78 | VALID | low | yes | two full read-modify-write cycles of `config.toml` per install (`setup-codex-hooks.sh:62-104,660-715`) |
| 79 | VALID | med | yes | step-2 write is bare `write_text`, no tmp+rename (all sibling writes are atomic) (`setup-codex-hooks.sh:104`) |
| 80 | VALID | low | fold→78/79 | unconditional rewrites bump mtime/inode when unchanged (`setup-codex-hooks.sh:104,686-715`) |

### F. OSC/metadata, branch detection, colors, launch

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 48 | VALID | med | yes | interceptor + alacritty both emit TitleChanged; payloads can differ on `;` (`metadata.rs:85-92`, vte `ansi.rs:1350-1359`) |
| 49 | VALID | low | yes | BEL emitted by both parsers (`metadata.rs:81-83`, alacritty `term/mod.rs:1437-1440`) |
| 50 | VALID | low | fold→48 | doubled registry write-locks + frames; client repaint deduped only for identical strings (`ipc_server.rs:8707-8817`) |
| 51 | VALID | low | fold→49 | duplicate Bell frames → double `request_attention` (`bell.rs:108-112`, `main.rs:2926-2928`) |
| 52 | VALID | med | yes | OSC 7 fires per prompt; no last-value suppression before registry/branch/workspace work (`metadata.rs:94-111`, `ipc_server.rs:8548-8551`) |
| 53 | VALID | low | no | `/proc` readlink deduped and cheap (`ipc_server.rs:8561-8568`) |
| 54 | VALID | med | yes | `.git/HEAD` walk per call, no cache (`ipc_server.rs:8619-8641`) |
| 55 | VALID | low | yes | branch detection runs then discarded when sink set empty (`ipc_server.rs:8828-8832,7299-7305`) |
| 56 | VALID | low-med | yes | per-pane independent `.git/HEAD` walk in ListSessions (`ipc_server.rs:6376-6396`) |
| 57 | VALID | med | yes | branch fs I/O under `live_sessions` + `workspace_manager` read guards (`ipc_server.rs:6360-6419`) |
| 75 | VALID | med | yes | color-query miss → full config read+parse per query; OSC 4 probe = 256 parses (`ipc_server.rs:8446-8465`, `config.rs:2075-2109`) |
| 76 | VALID | med | fold→75 | `resolve_theme` per query; external theme = second disk read (`ipc_server.rs:8465`, `config.rs:2148-2173`) |
| 81 | VALID | low | no | 3× `detect_shell` per launch, pure string match (`session_manager.rs:385-771`) |
| 82 | VALID | low | optional | 2× `find_scripts_dir` + repeated stats per launch (`shell_integration.rs:34-63`) |

### G. Shared-session fanout, synchronized output

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 58 | VALID | med | yes | `AttachedSinks` mutex held across per-sink awaits (`ipc_server.rs:7300-7304`) |
| 59 | VALID (local sinks only) | med | fold→58/21 | slow local sink stalls fan-out + reader; remote path already bounded (`ipc_server.rs:1345-1384,7279`) |
| 60 | VALID | low | no | N+1 copies per chunk; needs `Bytes` payload protocol change (`ipc_server.rs:1354-1355,7685`) |
| 61 | VALID | low | no | per-sink serialization, same trade-off (`framing.rs:50`, `ipc_server.rs:1493`) |
| 62 | VALID | med | yes | `drain_all_committed` empties queue every batch; no pacing anywhere (`main.rs:7264-7266`, `sync_frames.rs:193-202`) |
| 63 | VALID | med | fold→62 | `drain_until_frame` + catch-up threshold effectively dead code; contradicts lat.md/spec 016 (`sync_frames.rs:29,157-180`) |
| 64 | VALID | med | yes | full `make_content` viewport rebuild per intermediate frame, under `panes` mutex (`terminal.rs:208-215,612-631`) |

### H. Client IPC coalescing and search

| # | Verdict | Sev | Fix | Evidence anchor |
|---|---------|-----|-----|-----------------|
| 65 | VALID | med | yes | batch bounded by count/time only, never bytes (`ipc_bridge.rs:47-51,211-230`) |
| 66 | VALID | med | fold→65 | 100×64 KiB = 6.4 MiB per batch; single replay event can be tens of MiB (`ipc_server.rs:62`, `framing.rs:11`) |
| 67 | PARTIAL (cost attribution wrong) | low | fold→68 | memcpy fine; per-byte `SyncUpdateFrameSplitter` is the real cost (`sync_frames.rs:230-304`) |
| 68 | VALID | high | yes | full-batch VTE parse + grid snapshot under `panes`+sync mutexes render thread needs (`main.rs:7229-7267,3937`) |
| 69 | VALID | high | yes | per-keystroke full-history `ScreenSnapshot` clone (~48 MB at defaults), no debounce (`ipc_server.rs:6137`, `session_manager.rs:780-855`) |
| 70 | VALID | high | yes | snapshot AND scan inside Term lock; scan needs no lock (`ipc_server.rs:6136-6139`) |

## User Stories

### US1: Server survives misbehaving children and clients
As a Scribe user, I want session close, window close, and a stalled
client to never freeze other sessions, so that one wedged program or
paused client cannot take down my whole terminal server.
Covers: 5, 6, 7, 8, 9, 11, 12, 13, 21, 58/59.
**Acceptance criteria:**
- Closing a session/window whose child ignores SIGHUP completes without
  blocking the `live_sessions` write lock or a Tokio worker (child reap
  moved off-lock and off-worker, e.g. `spawn_blocking` or a watcher).
- A direct child-exit watcher reaps children and reports real exit
  codes/signals in `SessionExited`.
- Reader tasks have retained handles and a cancellation path; teardown
  is awaitable with a bound; an orphaned reader no longer hot-spins.
- A local client that stops reading its socket cannot back-pressure the
  PTY read loop or freeze the shared Term; sink fan-out no longer holds
  the attachment mutex across socket awaits.
- Verified with a manual scenario: SIGSTOP'd client + `yes` in another
  pane keeps other sessions live (constitution P3/P4).

### US2: Attach and replay are lossless and bounded
As a user reattaching or sharing a session, I want the replayed screen
to be complete and correct, so that no output is missing or wrongly
erased.
Covers: 3, 4, 17, 18, 19, 22, 23.
**Acceptance criteria:**
- No output gap between snapshot and sink install (install first, or
  buffer-and-flush the window); the ED2-erases-newer-bytes variant on
  re-point is also closed.
- Replay decompression bound derives from actual encoded size (or is
  streamed/size-prefixed), not 8 B/cell; a truecolor-dense screen
  round-trips through handoff without a blank session.
- Attach fan-out is deduplicated and concurrency-capped; replay
  encode/compress runs via `spawn_blocking`; client decompression no
  longer starves its current-thread runtime.
- Fresh sessions skip the redundant attach/replay and use real pane
  geometry (no 120×36 shrink/regrow, at most one SIGWINCH).

### US3: Client IPC is bounded and paced
As a user with a high-output pane, I want the client to stay responsive,
so that a `yes`-style firehose doesn't balloon memory or stall renders.
Covers: 14, 15, 62/63, 64, 65/66, 68 (+67 splitter cost).
**Acceptance criteria:**
- Inbound and outbound client queues are bounded with an explicit
  overflow policy (coalesce/drop-and-resync for output; cap and surface
  for outbound during reconnect).
- Batches are byte-bounded; VTE parse of large batches does not run
  while the render thread's `panes` mutex is needed (restructure lock
  scope or move parse off-lock).
- Committed-frame pacing matches the documented one-burst-per-redraw
  design: `drain_until_frame` is wired into the drain (per Q3; the plan
  defines where `SessionReplay` sits in paced ordering); invisible
  intermediate frames no longer rebuild full viewport content.
- The per-byte frame splitter no longer does `remove(0)` per byte.

### US4: Environment persistence actually works end to end
As a user enabling env persistence, I want fresh sessions, all supported
shells, and mid-session enablement to persist and restore correctly, so
the feature isn't silently inert.
Covers: 26 (helper resolution), 27, 28, 29, 30, 31, 32, 33, 34, 35, 72.
**Acceptance criteria:**
- Packaged installs resolve `scribe-hook-helper` (server injects an
  explicit path env var or PATH entry; deb ships nushell/pwsh scripts).
- Restore scripts are rendered per shell kind (fish `set -gx`/`set -e`,
  pwsh `.ps1` syntax, nu-safe encoding incl. `'\''` and multi-line).
- Fresh sessions carry an envelope id (client sends its minted
  launch id); first-envelope bootstrap works without a restart. The fix
  covers the GUI client, `scribe-cli`, and `scribe-test` create paths
  (per Q4b).
- Enable/disable of persistence takes effect in newly started shells;
  "restart or re-init required" is the documented semantic for running
  shells in both directions (per Q4a — no server→shell trigger).
- Shell hooks skip snapshot/diff work when persistence is disabled
  (server exports a gate env var / toggles at spawn).
- Fish and nu diffs drop O(N²) scans; nu stops leaking `XDG_DATA_DIRS`.

### US5: Hook pipeline is lean and doesn't leak secrets
As an AI-tool user, I want hook events to be cheap and private, so that
per-keystroke/tool-call hooks don't burn CPU or expose prompts and env
values to other local users.
Covers: 37, 38 (+40), 41, 42, 43, 44, 45, 46.
**Acceptance criteria:**
- Prompt and env payloads travel via stdin or 0600 temp file, never
  argv; no secret-bearing value appears in `/proc/*/cmdline`.
- Oversized payloads no longer silently vanish via E2BIG.
- Codex adapter consolidates JSON parsing to one interpreter run per
  event (or eliminates Python); ~8-12 execs per event drops measurably
  (state before/after exec counts — constitution P4).
- Unchanged AI context percentages are suppressed server-side (equality
  guard before `AiStateChanged`).
- Transient hook connections no longer contend with the 32 long-lived
  client slots (separate limit), and stalled pre-Hello locals time out.

### US6: Metadata processing is deduplicated and off the hot path
As a user, I want per-prompt OSC traffic to do minimal work, so titles,
bells, CWD, and branch info don't cost registry locks and disk I/O every
command.
Covers: 48/50, 49/51, 52, 54, 55, 56, 57, 73/74, 75/76.
**Acceptance criteria:**
- One TitleChanged and one Bell per sequence (single source of truth;
  `;`-containing titles no longer produce two different titles).
- Repeated OSC 7 values are suppressed before registry/branch/workspace
  work; `.git/HEAD` result is cached per (session, cwd) with
  invalidation; detached sessions skip branch detection.
- ListSessions resolves branches after dropping both registry guards.
- Empty filtered chunks send no PtyOutput frame.
- Dynamic-color queries hit a cached config/theme (invalidated on config
  reload), not disk, per query.

### US7: Session admission and handoff are correct
As an operator, I want the session cap and handoff cleanup to be sound,
so limits actually bound live sessions and no unrelated process gets
signaled.
Covers: 1, 2, 10, 24, 25.
**Acceptance criteria:**
- The 256 cap counts live sessions and is enforced atomically across
  concurrent creates (permit/semaphore pattern like `MAX_CONNECTIONS`).
- Handoff cleanup validates PID identity (start-time or pidfd) before
  SIGHUP.
- Shared-mode viewport debounce coalesces to a trailing timer (cancel or
  generation-check), and continuous drag settles to ≤ ~4 applies/sec
  instead of event rate.

### US8: Search doesn't stall the session
As a user typing in the find overlay, I want search to be cheap, so each
keystroke doesn't clone 48 MB under the Term lock.
Covers: 69, 70.
**Acceptance criteria:**
- Scan runs outside the Term lock (snapshot only under lock at most).
- Per Q5: ~150 ms client-side debounce, plus one snapshot reused across
  query edits while the overlay is open, invalidated on new session
  output; typing a 10-char query no longer performs 10 full clones.

### US9: Installer edits are atomic and minimal
As a user running hook setup, I want config edits to be crash-safe and
idempotent, so an interrupted install can't truncate my Codex config.
Covers: 78, 79 (+80 folded).
**Acceptance criteria:**
- All installer config writes use tmp+`os.replace`.
- One read-modify-write cycle per file per run; unchanged content is not
  rewritten (mtime/inode stable on re-run).

## Constraints

- Every finding was validated against the codebase before acceptance;
  the 82-item source list and per-item verdicts are recorded above.
  Invalid/not-worth-fixing items are explicit non-goals, not silently
  dropped (user requirement).
- Constitution P1: fixes must preserve crate boundaries (scribe-pty vs
  scribe-server vs scribe-client) and use typed errors; no cross-cutting
  helper sprawl.
- Constitution P2: no behavior changes to session continuity semantics;
  reattach/handoff guarantees must be preserved or strengthened.
- Constitution P3: each user story needs a user-reachable verification
  path; test code only where explicitly requested or where existing
  coverage must change.
- Constitution P4: hot-path fixes (US3, US5, US6, US8) should state a
  measurable before/after check (exec counts, lock-hold times, allocs)
  or mark it inapplicable.
- Constitution P5: #44 (secrets), #10 (PID reuse), #17 (LAN-reachable
  fan-out) are trust-boundary fixes and take priority defaults-safe
  designs.
- Constitution P7: NEVER restart the running Scribe server during this
  work; wire-protocol changes must remain handoff-compatible (current
  HANDOFF_VERSION = 6; receiver accepts N and N-1, handoff.rs:92,686); `lat.md/` must be updated (client.md:1052 and the
  frame-pacing sections are already stale per #63 and must be
  reconciled with whichever pacing decision is made).
- alacritty_terminal is vendored/pinned (`0.26.0-rc1`); fixes to
  `Pty::drop` behavior must wrap, not fork, unless unavoidable.
- Shell-integration fixes must work on bash, zsh, fish, nushell,
  PowerShell as packaged (deb + macOS DMG layouts).

## Open Questions

1. **Frame pacing (#62/#63):** wire `drain_until_frame` into the drain
   (restoring the documented design) or delete the pacing code and amend
   lat.md/spec 016? The pacing design was deliberate; recommend wiring
   it in, but this reverses a de-facto behavior.
2. **Env-persistence baseline on enable (#32):** server-initiated
   baseline request to running shells needs a new shell-side trigger
   (no wire exists today). Accept "documented restart required" instead,
   or build the trigger?
3. **Search redesign scope (#69):** debounce-only (cheap) vs incremental
   server-side search (bigger). Which scope?
4. **Exit-code plumbing (#13):** `SessionExited.exit_code` is already
   `Option<i32>` on the wire — is surfacing signal terminations (e.g.
   negative codes or a new field) wanted, or codes only?
5. **Helper resolution (#26):** prefer server-injected
   `SCRIBE_HOOK_HELPER` path env var (matches ai-hook wrappers) or
   PATH manipulation? Injected var recommended.
6. **#82 launch caching:** memoizing `find_scripts_dir` breaks dev-build
   hot-swapping of integration scripts. Cache anyway, cache only in
   packaged builds, or drop?
7. **Offline `CreateSession` (from #16):** a new-tab chord while
   disconnected enqueues a `CreateSession` that fires on reconnect —
   acceptable, or should non-attach-gated frames be pruned on redial?
8. **Prioritization:** ~45 task-bead-worthy findings. Fix everything in
   one epic, or split severity tiers (correctness/security first,
   perf-waste second) into separate waves?

## Clarifications

All seven critical questions were answered by the human on 2026-07-29;
every recommendation was approved as-is ("all A").

**Q1: Epic and wave structure?**
A: Approved. One epic; beads clustered by mechanism, waves ordered by
severity. Wave 0: capture P4 perf baselines at `b90c932` before any fix
merges. Wave 1 (MVP): data-loss + security + availability — US1, US2,
US4 correctness, #10/#17/#43/#44/#45, US9, #69/#70 lock fix. Wave 2:
hot-path perf polish — US3, US5 exec reduction, US6, shell-hook waste.
#1 and #2 land together (fixing #1 alone activates the #2 race).

**Q2: Live-server rollout/compat policy?**
A: Approved. All wire/handoff changes additive (`#[serde(default)]`);
`scribe-hook-helper` dual-accepts the old argv contract and the new
stdin/temp-file transport for one release; handoff-inherited sessions
are documented as exempt from the new child-reap/exit-code/PID-identity
guarantees (EOF-based cleanup remains for them).

**Q3: Frame pacing?**
A: Wire `drain_until_frame` into the drain, restoring the documented
one-burst-per-redraw design. The paced ordering of `SessionReplay`
relative to committed frames must be defined in the plan. lat.md and
spec 016 stay authoritative.

**Q4: Env-persistence semantics?**
A: Approved (a+b+c). (a) Enable/disable takes effect in newly started
shells only — "restart or re-init required" is the documented semantic
for both directions; no server→running-shell trigger is built. (b) The
envelope-id bootstrap fix covers the GUI client, `scribe-cli`, and
`scribe-test` create paths. (c) `SCRIBE_HOOK_HELPER` and the gate var
join the env-diff exclusion set.

**Q5: Search fix scope?**
A: Option (b): move the scan outside the Term lock, add ~150 ms
client-side debounce, and reuse one snapshot across query edits while
the find overlay is open, invalidated on new session output.

**Q6: Queue overflow policies?**
A: Confirmed. Outbound (user input): never drop per-frame; on cap, tear
the connection / refuse input with visible feedback. Inbound (output):
coalesce, then drop-and-resync via existing `RequestSnapshot`; client
detects its own overflow; resync is per-participant in shared sessions.

**Q8 (analyze gate, 2026-07-29): #82 launch-dir caching?**
A: Ratified. Cache `find_scripts_dir` in packaged builds only,
preserving dev-build hot-swap; #81 folds into it. Optional P3 item
attached to US6. The analyze gate also approved GO for bead creation.

**Q7: Numeric defaults?**
A: Approved as proposed; individual beads may refine with recorded
rationale. Replay decompression: 64 MiB absolute post-inflate ceiling,
streamed decode. Batch byte cap 1 MiB (oversize events processed
alone). Inbound queue 256 events; outbound 1024 frames. Attach fan-out:
dedup by session id, 8 concurrent replay builds. Teardown join bound
2 s then detach+log. Transient hook slots 16 (separate semaphore); 5 s
pre-Hello timeout on local connections. Git-branch cache keyed
(session, cwd), invalidated on OSC 7 change, 5 s TTL for ListSessions.
Search debounce 150 ms.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders) against the constitution and the codebase.

### Critical Questions (answer before planning)

1. **Epic and wave structure (absorbs OQ8).** One epic for
   traceability, with beads clustered by *mechanism* (shared
   lock/transport structure), not severity — severity orders the waves,
   mechanism decides membership. Proposed: Wave 0 captures P4 perf
   baselines at `b90c932`; Wave 1 (MVP) = data-loss + security +
   availability (US1, US2, US4 correctness, #10/#17/#43/#44/#45, US9,
   #69/#70 lock fix); Wave 2 = hot-path perf polish (US3, US5 exec
   reduction, US6, shell-hook waste). Note dependency hazard: a
   severity-only order ships #1 (med) before #2 (low) and *introduces*
   the reservation race. Approve this structure? — flagged by: scope,
   requirements, ambiguity, feasibility
2. **Live-server rollout and compatibility strategy.** The current
   handoff version is 6 (spec's original "v5" was wrong); pre-upgrade
   shells in live sessions keep the old integration functions in memory,
   so changing `scribe-hook-helper`'s argv contract (#42/#43/#44)
   silently kills env persistence and AI state for them unless the
   helper dual-accepts old argv + new stdin/file transport; the new
   child-exit watcher (#11/#13) cannot reap handoff-inherited children
   (they were reparented when the old server exited), and #10's PID
   identity check has no start-time data for inherited sessions.
   Proposed policy: all wire/handoff changes additive
   (`#[serde(default)]`), helper dual-accepts both transports for one
   release, handoff-inherited sessions are documented as exempt from the
   new reap/exit-code/PID-identity guarantees (EOF-based cleanup
   remains). Accept? — flagged by: stakeholders, gaps, feasibility
3. **Frame pacing (OQ1).** Wire `drain_until_frame` into the drain
   (restores the documented one-burst-per-redraw design in
   lat.md/client.md and spec 016, and makes #64's rebuild fix
   meaningful) or delete the pacing code and amend the docs + lat test
   spec (`lat.md/test.md:571`, `sync_frames.rs` tests). Recommendation:
   wire it in; note this changes de-facto behavior and where
   `SessionReplay` sits in paced ordering must be defined. — flagged
   by: ambiguity, gaps, feasibility
4. **Env-persistence semantics bundle (absorbs OQ2).** (a) Baseline on
   enable: there is no server→running-shell channel; a prompt-gated
   trigger is ~10× the work. Recommendation: "restart or re-init
   required" for both enable AND disable (the spawn-time gate env var
   cannot affect running shells in either direction — the spec's US4
   criteria are amended accordingly). (b) Scope of the envelope-id fix:
   GUI client only, or also `scribe-cli` and `scribe-test` create paths
   (both hardcode `None`; without them CLI sessions stay inert and the
   harness can't verify US4)? Recommendation: all three. (c) New
   injected vars (`SCRIBE_HOOK_HELPER`, the gate var) must join the
   env-diff exclusion set or they re-create #72. Confirm a/b/c. —
   flagged by: ambiguity, feasibility, stakeholders, gaps
5. **Search fix scope (OQ3).** Options: (a) minimal — move scan outside
   the Term lock + client-side debounce (~150 ms); (b) also reuse one
   snapshot across query edits while the overlay is open, invalidated on
   output; (c) full incremental server-side search. Note: debounce alone
   does not reduce the ~48 MB per-executed-query clone, only its
   frequency. Recommendation: (b). — flagged by: ambiguity,
   feasibility, scope
6. **Queue overflow policies (US3).** The outbound queue carries user
   *input* (keystrokes); any per-frame drop can execute a mangled
   command — policy must be all-or-nothing (cap → tear connection /
   refuse input with visible feedback), never silent drop. Inbound
   output overflow: coalesce, then drop-and-resync via the existing
   `RequestSnapshot`; the client detects its own overflow (no wire
   change). Shared sessions: resync affects only the overflowed
   participant. Confirm these policies. — flagged by: feasibility,
   stakeholders, ambiguity, gaps
7. **Numeric defaults ratification.** Proposed defaults (each bead may
   refine with rationale): replay decompression absolute ceiling 64 MiB
   post-inflate (matches `MAX_MESSAGE_SIZE`) with streamed decode —
   never peer-declared-size-only (P5); client batch byte cap 1 MiB
   (oversize events processed alone); inbound queue 256 events /
   outbound 1024 frames; attach fan-out: dedup by session id, cap 8
   concurrent replay builds; teardown join bound 2 s then detach+log;
   transient hook connection slots 16 (separate semaphore) + 5 s
   pre-Hello timeout on local connections; git-branch cache keyed
   (session, cwd), invalidated on OSC 7 change + 5 s TTL for
   ListSessions; search debounce 150 ms. Approve or adjust. — flagged
   by: ambiguity, requirements, gaps

### Non-Blocking Observations

- Success criteria need stable ids at planning time (spec 016's
  `US<n>-<m>` pattern) so finding→bead coverage is mechanically
  checkable; the plan step must emit a traceability map for all 82.
- P4 baselines are unrecoverable after fixes land — Wave 0 must capture
  exec counts / lock-hold / alloc measurements before Wave 1 merges.
- Verification harness for server-side fixes: the dev identity
  (`scribe-dev-server`, separate socket/runtime dir) and the docker e2e
  harness (`just e2e-func`) are the P3 path — the live server is never
  restarted (P7). Plan must state this per story.
- `scribe-test/src/daemon.rs` ignores `SessionReplay` and observes
  frames #3/#22/#73-74/#13 change; harness updates are in-scope
  collateral of those beads.
- Codex adapter consolidation (#37) invalidates the installer's
  trusted-command hashes; the beads must include installer migration
  and acceptance for already-registered hooks.
- Existing on-disk env envelopes have no GC once id-minting moves to
  the client; add a cleanup/GC criterion to the US4 bead set.
- Temp-file transport for #42-45 should follow the existing env-apply
  model (`XDG_RUNTIME_DIR`, 0600, unlink-after-read, 60 s defensive
  unlink).
- #82 currently belongs to no story: disposition at clarify (cache in
  packaged builds only, preserving dev hot-swap, or drop).
- deb dev-variant and macOS `Contents/Resources` layouts must be
  covered by the #26 helper-resolution criteria (four layouts total).
- MAX_SESSIONS enforcement must also count handoff-restored sessions
  (`restore_from_handoff` bypasses reservation today).
- #73 is caused by ED3/picker filters, not OSC interception; it sits in
  US6 for convenience but the fix lands in `apply_pty_filters`/send
  guard.
