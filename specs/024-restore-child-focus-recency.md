# restore-child-focus-recency

## Problem Statement

A cold restore can reopen terminal windows in separate client processes. The
plain-launch singleton still hands every later focus request to the process
that owns `client.sock`, where `RECENT_TERMINAL_WINDOW` tracks only windows in
that process. If the user most recently focused a `--restore-child` window, a
later plain launch raises the singleton owner's local window instead.

The client needs cross-process terminal focus recency so the most recently
focused eligible live window can win the existing plain-launch handoff. The
change must not weaken singleton launch suppression, consume restore state,
create sessions, or turn focus tracking into a server or network feature.

## Goals

- Record enough local recency information for a `--restore-child` window to
  win a later same-flavor plain-launch focus handoff when it was focused last.
- Treat every terminal window hosted by a `--restore-child` process as
  eligible, including windows opened after the initial restore claim.
- Route the handoff only to a live eligible terminal window and recover safely
  when recorded processes or windows have exited.
- Preserve the existing fast path: a duplicate plain launch exits successfully
  before GPUI startup, `ColdStart::resolve`, server attachment, restore claims,
  or session creation.
- Preserve cold-restore fan-out, singleton stale-socket recovery, updater
  replacement behavior, same-UID checks, and stable versus dev isolation.
- Keep the feature local and fully usable offline. Store no terminal content,
  pane content, command text, credentials, or remote-control data.
- Guarantee the initial behavior on X11, where the network-disabled Docker
  harness can prove OS-visible focus routing. Keep transport code portable.

## Non-Goals

- Changing server window assignment, session ownership, sharing modes, or the
  cold-restore snapshot and claim model.
- Adding a CLI flag, setting, window picker, or other visible focus UI.
- Sending focus recency through the Scribe server, a remote connection, LAN
  sharing, telemetry, or another network service.
- Allowing one OS user or one Scribe flavor to focus another user's or
  another flavor's windows.
- Including explicit `SCRIBE_JOIN_WINDOW` processes in cross-process recency.
- Searching ordered recency history after a stale target; fallback goes to the
  singleton owner's recent terminal window.
- Preserving cross-process recency across singleton-owner replacement, after
  all clients exit, or across reboot.
- Claiming Wayland or macOS runtime support before an authorized focus-routing
  harness exists for those platforms.
- Restarting or invoking the host's live Scribe runtime during verification.

## Backlog Inputs

- `scribe-l39m`, "Persist terminal recency when restore children must win
  handoffs," is the required source. It is an open P4 `ponytail-debt` chore
  with no structural parent and no `discovered-from` origin.
- Closed task `scribe-ylf5.2`, under closed epic `scribe-ylf5`, supplies the
  historical singleton contract and its deliberate process-local ceiling. It
  is context only and does not supply the target parent.

## Target Epic

Create a new epic. Do not reopen or parent this work under closed epic
`scribe-ylf5`; the source backlog has no active epic ancestry to inherit.

## User Stories

1. As a user restoring several terminal windows, I want the window I focused
   most recently to receive a later plain-launch handoff even when that window
   belongs to a `--restore-child` process.

   Acceptance criteria:

   - Given a singleton owner and at least one live restored child, focusing the
     child and then launching the same flavor plainly activates that child's
     window.
   - The duplicate launcher exits with status 0 and opens no GPUI window,
     claims no restore entry, connects no window backend, and creates no
     session.
   - Focusing an eligible owner-process window afterward makes that window win
     the next plain-launch handoff, proving recency can move in both directions.

2. As a user, I want focus recency to leave startup and cold restoration
   unchanged except for which live window receives focus.

   Acceptance criteria:

   - A plain launch with no live singleton owner starts normally, including
     cold restore and server startup behavior already covered by the harness.
   - Each `--restore-child` still claims at most one restore entry, never fans
     out another child, and remains exempt from singleton acquisition.
   - A duplicate plain launch still hands off before `ColdStart::resolve`, so
     no live owner's restore snapshot or index entry changes.
   - After all old clients exit during an update, the one bare replacement can
     reclaim the stale singleton socket and open as it does now.

3. As a user, I want stale or damaged recency state to fail closed without
   blocking terminal access or crossing identity boundaries.

   Acceptance criteria:

   - A target that exits or crashes cannot make a later plain launch hang,
     focus another user's process, or delete restore data.
   - Missing, truncated, incompatible, or otherwise unreadable recency state
     does not prevent a normal owner from serving focus handoffs or a new owner
     from starting.
   - Stable Scribe and `scribe-dev` keep independent recency domains rooted in
     their existing flavor-specific runtime or state directories.
   - The mechanism accepts no network input and stores only identifiers and
     ordering data needed for focus arbitration.
   - If the newest target cannot activate, the singleton owner's recent
     terminal window receives focus within the same bounded handoff.
   - After singleton-owner crash or replacement, cross-process recency resets;
     the new owner safely uses its process-local recent terminal window until
     eligible restore children publish new activation events.

4. As a maintainer, I want an end-to-end check that observes the user's focus
   result rather than only checking internal records.

   Acceptance criteria:

   - A Docker visual E2E case restores at least two windows into separate
     processes, activates a child, runs a plain launch, and proves the same
     child becomes the active OS window.
   - The case also proves no extra terminal window, session, or restore claim
     appears and that the original owner can win again after later activation.
   - A bounded stale-target phase proves the handoff returns without blocking
     after the recorded target exits and activates the singleton owner's recent
     terminal window.
   - Verification runs only through the repository's network-disabled Docker
     recipes and never starts, stops, or restarts Scribe on the host.
   - The initial runtime guarantee is X11. Linux and macOS builds must keep
     compiling, but compile evidence alone does not claim Wayland or macOS
     focus-routing support.

## Constraints

- Reuse the existing terminal singleton and activation observation paths. Do
  not add a second general singleton system or a server protocol solely for
  focus recency.
- Keep same-UID peer validation and flavor-specific path resolution. Any new
  local artifact must follow existing directory and file permission rules.
- Recency reads and writes must tolerate interruption and concurrent client
  processes without corrupting restore state or blocking window startup.
- The current exemptions for remote and LAN dial clients remain unchanged.
  Explicit `SCRIBE_JOIN_WINDOW` clients remain excluded.
- No migration is required because recency is ephemeral. Old children do not
  publish, old owners ignore new command kinds, and absent, malformed, or
  unknown messages degrade to the current owner-local focus handoff.
- Terminal render and input performance budgets are inapplicable because focus
  recency runs only on window activation and launch handoff, outside those hot
  paths. The plan must still name a bounded E2E handoff timeout and measure it
  in the Docker visual harness against the current singleton baseline.
- Update the relevant `lat.md` singleton, socket or persistence, and E2E test
  sections with the final design. `lat check` must pass.
- Follow local-first and operational safety rules: no required network access,
  no host runtime tests, no live-server restart, and no publication without
  explicit authority.

## Open Questions

None. Product clarification and the analysis-gate scope revision resolved all
questions needed for planning.

## Spec Review

### Resolved Product Questions

1. Should cross-process recency include only `--restore-child` clients, or also
   explicit `SCRIBE_JOIN_WINDOW` clients? Join windows may show deliberately
   shared sessions and would expand the lifecycle and security scope. Flagged
   by: requirements, gaps, ambiguity, feasibility, scope, stakeholders.
2. Within an eligible restore-child process, should every terminal window be
   eligible, or only the window opened from that process's restore claim?
   Process-wide eligibility reuses its existing recent-window handle; limiting
   eligibility needs new per-window origin tracking. Flagged by: requirements,
   ambiguity, scope.
3. If the newest cross-process target is stale or cannot activate, should the
   singleton owner's recent window receive focus, or should routing search an
   ordered history for the next live target? Searching history roughly doubles
   arbitration and cleanup work. Starting another client is excluded because
   it violates the singleton contract. Flagged by: requirements, gaps,
   ambiguity, feasibility, scope.
4. Must recency survive singleton-owner crash or replacement while restore
   children remain alive, or may the new owner reset to owner-local behavior?
   Continuity requires discovery or durable re-registration; reset permits a
   much smaller live broker. Flagged by: ambiguity, feasibility, stakeholders.
5. Is this behavior promised on every supported desktop, with Docker X11
   automation plus platform-appropriate manual or native evidence, or is the
   first release explicitly X11-only? The current automated visual harness
   cannot exercise Wayland or macOS. Flagged by: stakeholders.

### Technical Decisions (self-resolved; veto at the gate to override)

- Keep the existing terminal singleton owner as the broker. The plain launcher
  still writes the existing `focus` command to `client.sock` and exits before
  GPUI startup or cold-start resolution.
- Give each eligible client process one short, generation-tagged Unix focus
  endpoint in the flavor runtime directory. Route to that process's existing
  recent terminal handle instead of creating per-window sockets.
- Authenticate recency publishers with same-UID peer checks plus the existing
  executable-identity pattern used by server handoff. Use full `WindowId`
  values and random generations; never trust display ids or PID reuse.
- Let the broker serialize positive GPUI activation reports and assign an
  increasing in-memory sequence. Receipt order breaks simultaneous ties; wall
  clocks and PID ordering are not used.
- Count normal OS activation, including activation caused by a successful
  handoff, as focus. Do not synthesize extra terminal focus reports. Initial
  mapping alone does not outrank a later positive activation.
- Keep the registry in memory and use generation-checked orderly cleanup plus
  lazy failed-target pruning. A new singleton owner starts a fresh registry and
  dispatches one bounded background scan for strict-prefix endpoint sockets.
  The scan keeps live authenticated endpoints and removes only proven-dead
  sockets. It is not a sweeper and does not delay GPUI or cold-start work.
- Keep routing asynchronous and bounded. Use a 100 ms target IPC timeout, fall
  back to the singleton owner's recent terminal, and require observed
  activation within 2 seconds and no more than 500 ms slower than the
  owner-only baseline in the same visual container.
- Keep mixed-version behavior backward compatible without migration. Old
  children do not publish; absent, malformed, or unknown state degrades to the
  current owner-local handoff. Recency I/O failure never blocks normal startup.
- Use no server message, network path, setting, CLI flag, daemon, repair tool,
  background sweeper, or new dependency. Store and log no terminal content.
- Add one network-disabled Docker visual E2E scenario with distinct
  process-to-window evidence. Assert the chosen OS window, unchanged client,
  session, PTY, and restore-claim counts, both recency directions, and bounded
  stale-target fallback.

### Non-Blocking Observations

- Minimized windows and windows on another monitor or virtual desktop remain
  eligible when GPUI can activate them, matching existing handoff behavior.
- Pane and tab recency, settings-window focus, remote and LAN clients, server
  propagation, telemetry, and cross-reboot recency remain out of scope.
- Corrupt or incompatible ephemeral state should emit a structured warning;
  expected absence and stale pruning need only debug logging.
- The visual E2E must create one singleton owner and at least one real
  `--restore-child` process. Existing restore tests do not yet prove that exact
  topology.

## Clarifications

**Q1: Which clients participate?**

A: Only `--restore-child` processes. Explicit `SCRIBE_JOIN_WINDOW`, remote, and
LAN clients remain excluded.

**Q2: Which windows in a restore-child process participate?**

A: Every terminal window hosted by an eligible restore-child process uses that
process's existing most-recent-window tracking.

**Q3: What happens when the newest target is stale?**

A: The singleton owner's recent terminal window receives focus. The broker
does not retain or search ordered target history and never opens another client.

**Q4: Should recency survive singleton-owner replacement?**

A: No. A new owner starts with owner-local recency; live restore children may
become eligible again only after publishing new activation events.

**Q5: What platform scope should ship?**

A: Initial guarantee is X11. This revises the earlier all-desktop answer at the
analysis gate because no authorized Wayland or macOS focus harness exists.
Transport code remains portable, but build success is not a runtime claim.

## Architecture Approach

Keep `client.sock` as the only terminal singleton and make its owner the live
focus broker. A plain duplicate launch continues to send the existing `focus`
command and exit before `ColdStart::resolve`. No server connection, restore
claim, GPUI startup, or session creation moves ahead of that return.

Each `--restore-child` process binds one short, generation-tagged Unix socket in
the flavor runtime directory after GPUI starts. The endpoint represents the
process, not one window. Its activation handler therefore reuses
`RECENT_TERMINAL_WINDOW`, and every terminal window later opened by that process
participates without origin tracking or per-window sockets.

Positive OS activation follows one of two paths:

- An owner-process window replaces the broker winner with the owner's current
  local handle.
- A restore-child window sends an activation announcement to `client.sock`.
  The owner verifies the peer UID, executable identity, `--restore-child` role,
  generation, and endpoint path before replacing the winner.

The owner serializes accepted activation events and assigns an increasing
in-memory sequence. Receipt order is the deterministic tie-breaker. The broker
retains only the current winner, either owner-local or one restore-child
endpoint, because stale fallback never searches history.

On a duplicate-launch focus command, the broker activates its local recent
window or sends a bounded request to the selected child endpoint. A child
reports success only when its current `WindowHandle` still updates and accepts
`window.activate_window()`. Missing endpoint, timeout, generation mismatch, or
unavailable handle prunes that winner and activates the owner's recent window
within the same handoff. The launcher remains fire-and-exit and does not wait
for OS activation.

Endpoint closure and failed routing provide lazy cleanup. Orderly process exit
removes a socket only when its path and generation still match that process.
A crash can leave an inode. After binding `client.sock`, a new singleton owner
dispatches one background scan for at most 64 names with the exact
`client-focus-` prefix and stops after 500 ms. The scan probes matching sockets,
keeps live endpoints that authenticate with the expected generation, leaves
indeterminate live entries untouched, and removes proven-dead sockets. It never delays
`prepare_terminal_singleton`, GPUI startup, or `ColdStart::resolve`, and it does
not repeat. Singleton-owner replacement still discards broker ordering. Live
children participate again only after a new positive activation report.

The plan satisfies the constitution as follows:

| Principle | Plan check |
| --- | --- |
| Clear Boundaries and Typed Failure | Terminal focus transport stays in the client singleton code; typed commands and results distinguish rejection, timeout, unavailable window, and accepted activation. |
| Session-Safe, Consistent UX | Existing activation observation, owner-local fallback, singleton launch suppression, restore claims, and server-owned sessions remain unchanged. |
| Explicit, Risk-Based Verification | Unit coverage checks arbitration and malformed IPC; Docker visual coverage proves the OS-visible child and fallback results. |
| Performance Budgets and Measurement | Target IPC is bounded at 100 ms; Docker measures activation within 2 seconds and no more than 500 ms behind the owner-only baseline. |
| Default-Safe Trust Boundaries | Private runtime paths, 0600 sockets, same-UID checks, executable checks, restore-child role checks, and generation matching reject untrusted targets. |
| Local-First Data Locality | All state and IPC remain local and ephemeral; no terminal content or network message is introduced. |
| Compatible, Documented, Operationally Safe Change | Existing `focus` framing stays valid, rollback ignores endpoint artifacts, one bounded scan reclaims crash debris, each task updates its `lat.md` sections, runtime tests stay in network-disabled Docker, and no live server is restarted. |

## Affected Components

| Component | Planned change |
| --- | --- |
| `crates/scribe-client/src/main.rs` | Replace the owner-only focus channel with broker events, publish positive activation from `TerminalView::on_activation`, start and retire restore-child endpoints, route duplicate focus requests, and preserve startup ordering around `prepare_terminal_singleton` and `ColdStart::resolve`. |
| `crates/scribe-client/src/settings/singleton.rs` | Add terminal-specific command framing, bounded endpoint request and acknowledgement helpers, private socket binding, and peer identity checks. Keep settings singleton behavior and the existing plain `focus` payload intact. |
| `crates/scribe-common/src/socket.rs` | Add flavor-scoped helpers for short generation-tagged focus endpoint paths while retaining the existing 0700 runtime directory convention. |
| `crates/scribe-client/src/restore_replay.rs` | Keep restore-child detection and fan-out behavior unchanged; use `RESTORE_CHILD_ARG` as the eligibility role checked by the client singleton transport. |
| `tests/e2e/visual/relaunch-common.bash` | Reuse or extend bounded window, process, snapshot, and server-state helpers needed by the new visual scenario. |
| `tests/e2e/visual/restore-child-focus-recency.sh` | Add the real cold-restore, cross-process focus, reverse-recency, timing, and stale-target oracle. |
| `justfile` | Add a network-disabled purpose-built visual recipe and register the script in `e2e-all-visual`. |
| `lat.md/client.md`, `lat.md/common.md`, `lat.md/test.md` | Document broker ownership, endpoint paths and trust checks, fallback behavior, performance bounds, and test coverage. |

`crates/scribe-common/src/protocol.rs`, server window assignment, restore-state
files, settings UI, CLI flags, and dependencies remain unchanged.
`crates/scribe-server/src/handoff.rs` is reference-only: its `SO_PEERCRED` or
`LOCAL_PEERPID`, command-line, executable-path, and canonical-path checks are
the existing authentication pattern, but the server crate is not affected.

## Data Model

All new recency state is process memory or a bound Unix socket. Nothing is
written to the restore store or another durable file.

| Model | Fields and rules |
| --- | --- |
| Restore-child endpoint identity | Random generation, short flavor-scoped socket path, and current process identity. The generation prevents PID reuse or a stale inode from impersonating a newer endpoint. |
| Activation announcement | Command kind, generation, endpoint path, and full `WindowId` for diagnostics and test correlation. It contains no title, terminal bytes, pane data, command text, or credentials. |
| Broker winner | Monotonic in-memory sequence plus `Owner` or `RestoreChild { generation, endpoint, window_id }`. Only one winner is retained. |
| Endpoint activation request | Command kind and expected generation. The endpoint rejects a request for another generation. |
| Endpoint result | `Activated` or a typed rejection such as generation mismatch or unavailable window. Transport timeout and disconnect remain distinct I/O failures at the broker. |
| Endpoint cleanup | Orderly cleanup carries the process generation and removes only its exact socket. Owner startup scans only the strict `client-focus-` prefix once, checks at most 64 entries for at most 500 ms, keeps live authenticated generations, and removes proven-dead sockets. |

The broker starts owner-local. An owner activation sets `Owner`; an accepted
child announcement sets `RestoreChild`. Failed child routing removes only the
matching generation and falls back to owner-local. A new singleton owner starts
with no external winner, regardless of stale child sockets.

## API / Interface Changes

- Keep the existing newline-delimited JSON `focus` command accepted by
  `client.sock`. Older launchers and older owners retain current behavior.
- Add private terminal singleton commands for restore-child activation
  announcements, endpoint activation requests, and activation results. Unknown,
  truncated, oversized, or malformed commands are rejected without changing
  the current winner.
- Add a `client_focus_socket_path(generation)` style helper in
  `scribe-common::socket`. It must produce a short name under the current
  flavor runtime directory, never accept an arbitrary path from a publisher,
  and keep the directory at 0700 and socket at 0600.
- Extend client peer inspection using the existing server handoff pattern:
  kernel-derived UID and PID, current-flavor client executable comparison, and
  `--restore-child` command-line verification for publishers. Both endpoint
  sides verify peer identity before accepting a command.
- Keep command reads and target acknowledgement bounded at 100 ms. Publication
  failure is a warning or debug event and never blocks GPUI activation or
  startup.
- After singleton acquisition, dispatch one background cleanup pass that scans
  only `client-focus-` names and stops after 64 entries or after 500 ms.
  Probe before unlinking, preserve live authenticated endpoints, remove
  proven-dead sockets, and leave indeterminate live entries alone. Do not add a
  timer, watcher, sweeper, or durable registry.
- Add internal broker input for owner activation, child activation, and plain
  focus requests. The public CLI, environment variables, server wire protocol,
  settings schema, and restore schema do not change.
- Treat mixed versions as safe degradation. Old children never announce, old
  owners ignore new command kinds, and new owners with no valid announcement
  use their local recent window. No migration or repair API is needed.

## Testing Strategy

Add focused unit coverage beside the changed code:

- Broker ordering accepts owner and child events in receipt order, retains only
  the winner, resets on new-owner construction, and prunes only the failed
  generation.
- Command parsing keeps the old `focus` frame compatible and rejects unknown,
  truncated, oversized, malformed, wrong-generation, wrong-role, wrong-flavor,
  and wrong-executable input without changing broker state.
- Endpoint tests cover successful activation acknowledgement, a live process
  with no valid window handle, disconnect, and the 100 ms timeout.
- Path tests prove stable and dev isolation, short socket names, 0700 parent
  permissions, and 0600 endpoint permissions on supported Unix platforms.
- Cleanup tests leave endpoint sockets behind after a simulated crash, prove a
  one-shot owner-replacement scan preserves live authenticated generations and
  removes dead ones, prove orderly cleanup cannot unlink a replacement
  generation, and prove a rollback-style legacy focus acquisition ignores the
  new strict-prefix artifacts.
- Startup tests retain the current restore-child, explicit join, remote, and LAN
  singleton exemptions and prove duplicate launch returns before cold-start
  resolution.

Add `tests/e2e/visual/restore-child-focus-recency.sh` by combining the proven
setup from `cold-restart.sh`, `multi-window-restore.sh`, and
`relaunch-focus.sh`. The recipe must not set `SCRIBE_SHARE_TAP`; the tap cannot
survive the required server restart. The network-disabled scenario must:

- Fail early unless the visual image has `xdotool` with `getwindowpid`, then
  create two replayable windows, stop only the disposable container runtime,
  relaunch into one owner and one real `--restore-child`, map each visible X11
  window to a PID with `xdotool getwindowpid`, and verify both PIDs against the
  container's live `scribe-client` processes.
- Measure an owner-only focus handoff in the same container as the baseline.
- Focus the child, place an unrelated X window in front, run a plain launch,
  and prove the same child window becomes active within 2 seconds and no more
  than 500 ms slower than the baseline.
- Prove the duplicate exits 0 and does not add a GPUI window or client process.
  Count server log events named `client identified via Hello` and
  `created new PTY session`; hash the restore index and every restore window
  file before and after each duplicate launch; require unchanged counts and
  hashes.
- Focus an owner window and prove the next plain launch selects the owner.
- Focus the child again, terminate only that child, and prove the next plain
  launch falls back to the owner within the same 2-second bound.
- End with an updater-shaped regression: terminate all old clients, leave crash
  endpoint debris, launch one bare replacement, and prove it reclaims stale
  `client.sock`, starts one owner, removes proven-dead strict-prefix sockets,
  and neither creates duplicate sessions nor consumes another restore claim.
- Retain the existing `--network none` Docker invocation and an explicit suite
  timeout. Never invoke a Scribe runtime on the host.

Write all evidence beneath
`test-output/restore-child-focus-recency/`, mounted as
`/output/restore-child-focus-recency/` in the container. Required artifacts are
`owner-baseline.json`, `child-handoff.json`, `owner-return.json`,
`stale-fallback.json`, `updater-reclaim.json`, `restore-before.sha256`,
`restore-after.sha256`, per-process logs, and screenshots for child selection
and owner fallback.

Named automated validation is `just docker-visual`, followed by the new
`just e2e-visual-restore-child-focus-recency`, the existing
`just e2e-visual-relaunch-focus`, and `just e2e-visual-cold-restart`. Run
`lat check` and `git diff --check` after documentation changes. The current
runtime harness has no unit-test entry point, so the implementation agent must
not substitute host `cargo test`. Execution of the new Rust unit tests depends
on the exact `rust-linux` and `rust-macos` jobs in
`.github/workflows/quality.yml` after a user-authorized push.

Docker X11 supplies the runtime evidence for the initial guarantee.
`docker/Dockerfile.visual` has no Wayland path, and
`.github/workflows/native-macos-metal.yml` covers only the terminal-images
Metal corpus. Linux and macOS compilation remain portability checks, not
Wayland or macOS runtime evidence.

## Risks

| Risk | Mitigation |
| --- | --- |
| A target exits between announcement and focus | Generation match, 100 ms transport bound, lazy pruning, and owner-local fallback make the race harmless. |
| A live process has closed its recent window | The endpoint acknowledges only a successful handle update; unavailable handles return a typed failure and trigger owner fallback. |
| A same-UID process forges an endpoint | Validate peer PID, executable path, restore-child argument, generation, flavor runtime parent, and socket permissions before registry mutation or activation. |
| Concurrent activation reports reorder | The owner assigns sequence in listener receipt order. No wall clock, PID order, or cross-process counter participates. |
| Activation IPC stalls GPUI | Publish outside the synchronous window callback and bound every read, write, and acknowledgement. Drop failed reports rather than delaying focus or startup. |
| Owner replacement leaves child sockets alive | Registry state is intentionally lost. Stale sockets cannot win until a child publishes a new positive activation to the new owner. |
| Crash debris accumulates across owner replacement | Run one bounded strict-prefix scan on the singleton background thread, keep live authenticated endpoints, remove proven-dead sockets, and cover crash, owner replacement, rollback, and updater reclaim. No sweeper is added. |
| Old and new client binaries overlap during update | Existing `focus` stays compatible; unknown commands degrade to owner-local behavior and no durable migration exists. |
| Unix socket paths exceed platform limits | Use a short generation-derived filename under the existing runtime directory rather than PID plus full UUID text. |
| Visual timing becomes flaky | Measure owner and child paths in one container, poll the active OS window, use the 2-second correctness ceiling, and compare against a generous 500 ms delta. |
| Portable code is mistaken for cross-platform runtime proof | State the initial X11 guarantee in docs and release evidence; treat Linux/macOS compilation as portability checks only and defer broader claims until authorized harnesses exist. |

## Sequencing

- **Implement authenticated terminal recency transport (P1).**
  Depends on: none.
  Acceptance: `settings/singleton.rs` and `socket.rs` bind one private,
  generation-tagged endpoint per restore-child; typed announcements, activation
  requests, and results use 100 ms bounds; peer UID, PID, executable, role,
  flavor, and generation checks fail closed; the existing plain `focus` command
  remains byte-compatible; no dependency, server protocol, or durable file is
  added; targeted unit tests cover framing, authentication decisions, timeout,
  malformed input, permissions, flavor isolation, crash debris, a live endpoint
  preserved across owner replacement, generation-checked orderly cleanup, and
  rollback-style legacy acquisition. Update `lat.md/common.md` and the terminal
  singleton unit-test sections in `lat.md/test.md`; run `lat check`.

- **Wire process-wide recency into GPUI focus handoff (P1).**
  Depends on: Implement authenticated terminal recency transport.
  Acceptance: owner and restore-child positive activation events update one
  owner-serialized in-memory winner; every terminal window in a restore-child
  process uses its process-local recent handle; external activation success is
  acknowledged; stale, missing, or unavailable targets fall back to the owner's
  recent window; owner replacement resets external recency; duplicate launch
  still exits 0 before `ColdStart::resolve`, server attachment, restore claim,
  GPUI startup, or session creation. Broker tests explicitly cover receipt-order
  winner selection, owner replacement reset, stale external pruning, and
  owner-local fallback. `just docker-visual` and
  `just e2e-visual-relaunch-focus` provide the Docker smoke. Add the Rust unit
  tests now, but record that their execution depends on the exact `rust-linux`
  and `rust-macos` CI jobs after user-authorized push because the current
  runtime harness has no unit-test entry point. Update `lat.md/client.md` and
  the broker unit-test sections in `lat.md/test.md`; run `lat check`.

- **Prove restore-child focus routing in Docker (P1).**
  Depends on: Wire process-wide recency into GPUI focus handoff.
  Acceptance: a purpose-built `--network none` visual recipe runs without
  `SCRIBE_SHARE_TAP`, verifies `xdotool getwindowpid`, creates distinct owner
  and restore-child processes, proves child then owner recency, proves
  stale-child owner fallback, and records the owner baseline and child timing.
  Server `client identified via Hello` and `created new PTY session` counts plus
  before-and-after restore hashes prove no extra connection, PTY, restore claim,
  or restore mutation. An updater-shaped final phase proves a bare replacement
  reclaims `client.sock` and dead endpoint debris without duplication. Store the
  named JSON, hash, log, and screenshot artifacts under
  `test-output/restore-child-focus-recency/`. The new case, `relaunch-focus`,
  and `cold-restart` pass through Docker only. Update the visual E2E sections in
  `lat.md/test.md`; run `lat check`.

- **Record X11 evidence and portability status (P2).**
  Depends on: Prove restore-child focus routing in Docker.
  Acceptance: collect the Docker X11 artifacts and, after user-authorized push,
  the exact `rust-linux` and `rust-macos` CI results; document X11 as the only
  runtime guarantee and those jobs as portability checks; record Wayland and
  macOS runtime validation as deferred until authorized harnesses exist. This
  item adds no product code, test harness, or architecture documentation.

## Backlog Refinement

Create a new epic named `Restore child focus recency`; do not reuse closed epic
`scribe-ylf5`. Place the four sequencing work items under it with the stated
priorities and dependencies.

Disposition for source `scribe-l39m`: `split-and-supersede`. The source is too
broad for one independently verifiable implementation bead. Supersede it only
after all replacement beads exist and carry these coverage links:

- `Implement authenticated terminal recency transport` covers private endpoint
  creation, trust checks, compatible framing, bounded failures, and flavor
  isolation.
- `Wire process-wide recency into GPUI focus handoff` covers cross-process
  ordering, all windows in eligible restore-child processes, owner fallback,
  owner-replacement reset, and the unchanged duplicate-launch fast path.
- `Prove restore-child focus routing in Docker` covers the user-visible focus
  result, distinct process topology, session and restore non-regression,
  performance budget, reverse recency, and stale-target fallback.
- `Record X11 evidence and portability status` collects the X11 runtime proof
  and CI portability evidence. Architecture and test documentation are already
  owned by the first three replacements.

The epic is complete only when every replacement is closed, the source has
been superseded by those replacements, no descendant or direct source remains
open at P4, Docker X11 evidence is recorded, and `lat check` passes.

## Alignment fixes applied

- A-must: replaced the unresolved stale-target and migration wording with the
  approved owner-local fallback and no-migration downgrade rules; retained the
  historical questions under `Resolved Product Questions` and Clarifications.
- A-must: specified generation-checked orderly cleanup plus one bounded,
  strict-prefix owner-start scan that preserves live authenticated endpoints,
  removes proven-dead sockets, and never becomes a sweeper.
- A-must: made the visual oracle executable without `SCRIBE_SHARE_TAP`, using
  verified `xdotool getwindowpid`, exact server Hello and PTY log events,
  restore hashes, and named artifacts under
  `test-output/restore-child-focus-recency/`.
- A-must: stated the real test boundary. Rust unit execution waits for the
  exact Linux and macOS quality jobs after user-authorized push because the
  Docker runtime harness has no unit-test entry point.
- B-should: moved `handoff.rs` to reference-only status, distributed `lat.md`
  updates and `lat check` across the first three work items, simplified task
  dependencies, and added crash, replacement, rollback, and updater-reclaim
  regressions.
- Human blocker resolved: the initial guarantee is X11. Wayland and macOS
  runtime claims remain deferred until authorized focus harnesses exist.

## Analysis gate fixes applied

- Revised platform scope from all supported desktops to an initial X11 runtime
  guarantee at the user's request.
- Removed unexecutable Wayland and macOS evidence placeholders. Linux and macOS
  CI remain portability checks and cannot satisfy runtime acceptance.
