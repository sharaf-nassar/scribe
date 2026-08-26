# gh-ci-run-bar research

Research artifact for `specs/023-gh-ci-run-bar.md`. Records the completed
Story 1 ownership and transport findings plus the Story 2 UX design rules.
GitHub claims use current official documentation; temporary fixtures verify
the Git ref backends supported by the host Git version.

## Ownership conclusion (Story 1 milestone)

**Conclusion: server-owned tracking.** The CI tracker lives in
`scribe-server`; clients only render state received over IPC.

Topology. Sessions are server-owned and long-lived; the repo checkout,
its `.git` ref state, and the user's `gh` auth all live on the machine
running the server. Under remote window control and LAN sharing, the
viewing client is on a different machine that has neither the repo nor
(necessarily) `gh` credentials — client-owned tracking is broken by
construction the moment a window is attached remotely, while server-owned
tracking works identically for local, remote, and shared viewers: the bar
is just rendered state.

Deduplication. One tracker per (host, repo) per server is only enforceable
server-side. N windows, workspaces, or attached clients viewing the same
repo share one polling window and one dismissal state; client-owned
trackers would multiply GitHub traffic against one rate-limit budget and
desync dismissal.

Crate boundaries (constitution #1). The tracker sits beside the updater in
`scribe-server` — the one module that already speaks GitHub REST
(`updater.rs`, reqwest, env-overridable API base URL). Protocol additions
go in `scribe-common/src/protocol.rs`; rendering goes in `scribe-client`.
No new cross-cutting helpers.

Hot-upgrade continuity (constitution #2/#7). Tracker state is small and
re-derivable: an active window is (repo, head SHA, deadline, last run
snapshot). The upgrade handoff either carries these descriptors alongside
session state or the new server re-polls once per window that was active
at handoff, then resumes the normal cadence. Steady state after upgrade
remains zero requests; a run that both starts and finishes entirely
inside the handoff gap is missed, which is within the own-push v1 promise
(the push predates the upgrade, so a window existed and is re-polled).

Token containment. `gh auth token` is invoked by the server at need; the
token never enters IPC messages, logs, or remote/LAN transports, and is
only sent to the API host `gh` authenticated (constitution #5/#6).

## UX design rules (Story 2)

### Prior art

**VS Code GitHub Actions extension.** Sidebar tree of workflow runs with
per-job/step expansion and streaming logs; a status-bar entry summarizes
the current branch's latest run. Polling-based refresh. Takeaway: the
split between an always-visible one-line summary and an opt-in detail
tree maps directly to our collapsed bar + expandable panel; per-step
streaming is overkill for a terminal's ambient surface.

**GitHub Desktop.** PR/branch header shows a single rollup glyph
(spinner/check/cross) with a popover listing each check and its state,
plus a desktop notification on completion. Takeaway: worst-status rollup
with per-check chips is instantly readable; terminal states must linger —
Desktop's notification exists precisely because transient status gets
missed.

**`gh run watch`.** Full-screen live view refreshing every ~3 s: run
title, per-job lines with `✓`/`X`/`*` glyphs and elapsed time, final
summary line on exit. Takeaway: glyph + text + elapsed is sufficient
realtime feedback at terminal density; a few seconds of update latency
feels live.

### Bar state machine

States: `hidden → running → (success | failure | cancelled) → hidden`,
with `stale` as an overlay of `running`.

- **hidden** — no tracked run. Also the state when the feature is
  disabled, prerequisites are missing, or a push-gated window closes
  without ever observing a run (a repo with no Actions never flashes a
  bar; the window times out silently).
- **running** — first observation of a workflow run for the pushed head
  is the "CI triggered" notification: the bar appears. Shows aggregate
  glyph `◐` + "running", per-workflow chips, branch @ short-SHA, elapsed.
  Queued runs render as chips with `◌` + "queued" inside running.
- **success / failure / cancelled** — terminal aggregate (worst-status:
  any failure → failure; else any cancelled → cancelled; else success).
  `✓ passed` / `✕ failed` / `⊘ cancelled`. Success auto-dismisses after
  ~10 s; failure and cancelled persist until dismissed — failure is the
  state the user must not miss.
- **stale** — API failure (auth expired, rate-limited, offline) during an
  active tracked run: keep the last-known content, lead with the state
  word `stale · retry in Ns`, stop advancing elapsed. Recovery resumes
  running; window deadline expiry goes to hidden. Outside an active run,
  failures never surface a bar.
- **dismiss** — `✕` hides the bar for the current tracked head and syncs
  across all attached clients; a new push (new head) resurrects the bar.
  Terminal states also auto-clear when the window closes.

Every state pairs a glyph with a word — color alone never carries the
status (accessibility rule from the spec review).

### Placement rules

- The bar is a **workspace-region chrome band**: it renders at the top of
  the workspace region whose repo owns the tracked run — directly under
  the titlebar for a single-workspace window or a top-row region, and
  directly under the region tab bar for a stacked region. This follows
  the existing dynamic-band precedent (per-pane prompt bar): panes shrink
  through the same content-rect rule, and the grid re-measures when the
  band appears or disappears.
- The band is region-wide, above panes — never per-pane, and it does not
  interact with the per-pane prompt bar (which stays inside pane content).
- Multi-window: every window showing a region rooted in the repo renders
  the same bar state; dismissal syncs. Two workspaces rooted in the same
  repo show the same tracker's state.
- Splits: the band spans the region's full width above all panes in that
  region.
- Shared viewers (LAN share, remote window control): the bar renders
  read-only — same content, with the action cluster (open-in-browser,
  dismiss) omitted. No token or credential is involved in rendering.
- Expanded panel: opens below the collapsed bar as part of the same band
  (pushing pane content down), listing each workflow's jobs with status
  glyph, current step, and elapsed. Toggled by click/keyboard on the bar;
  per-job data is fetched only while the panel is open.

### Edge-case run rules

- **Force push / rapid successive pushes**: the tracker follows the
  latest pushed head — the old window is dropped and a new one opens.
  The bar always describes exactly one head, shown as branch @ short-SHA.
- **Manual re-run**: a re-run of a tracked run observed inside an open
  window re-enters running. A re-run after the window closed is invisible
  (documented own-push v1 limitation).
- **Push that triggers no workflows**: no bar, ever — the window times
  out silently. First run observation, not push detection, is the
  user-visible trigger.
- **Multiple workflows**: all runs for the pushed head aggregate into the
  worst-status rollup; each workflow is an individual chip.

### Visual direction

The mockup's primary direction is **trace**: the band is a timeline, not a
status row. The 40px collapsed band is one aligned system — state
cluster, job cells, metadata. A conic ring fills as jobs complete and
collapses to a solid square mark on terminal states. Each job is a cell
(name over a live track); a wider gap marks a workflow boundary. The
active job is marked three ways at once: its track shimmers,
its name holds full foreground, and a breathing square dot (the
workspace-pill dot, smaller) precedes it; done jobs settle solid, queued
jobs dash, a failed job's name goes red. Beyond ~6 jobs the strip elides
to failed + active jobs plus a "+n" counter cell. Metadata groups as
`branch @ sha · elapsed` with a one-pixel rule before the actions.
Expanded, jobs render as time-positioned bars on a shared minute-grid
axis so parallelism reads at a glance. A hairline under the band takes
the workspace badge color — ownership encoded structurally, which is
what disambiguates the band in multi-workspace windows. Failure/stale
re-key that hairline semantically; stale leads with its own state word
("stale · retry in Ns") and freezes elapsed and all motion while keeping
the active job named. Motion (appear sweep, shimmer, one shared 1.2 s
breathing rhythm, success exit) routes through the existing GPUI
animation system and its off switch; terminal states are static. An
alternate **edge** direction trades the band for a 2px luminous progress
line under the titlebar plus a fading micro cluster naming the active
job — zero chrome height; both directions share the trace panel.

### Notification surface decision

The bar's appearance is the v1 notification. The client's existing
desktop-notification wiring (bell path) is the natural post-v1 opt-in
for run completion, but v1 adds no desktop notifications (spec
Non-Goal); no other in-app surface is repurposed.

## Transport and trigger verification (Story 1 remainder)

Verified against current documentation and temporary Git fixtures on
2026-08-14. The default should be push-gated conditional REST polling.
Nothing else meets the local-first, no-hosted-service, and zero-idle-request
constraints at the same time.

### Numeric contract

- **Steady state:** 0 GitHub HTTP requests. Do not run an API auth probe when
  enabling the setting or while idle. `gh auth token` and the first API call
  happen only after a qualifying local ref change.
- **Detection:** debounce ref events for 250 ms, issue the first run-list GET
  within 1 s, then poll every 5 s. A run that GitHub's REST API can already
  see appears within 5 s plus request time; the product acceptance bound is
  10 s.
- **Discovery window:** 120 s after the latest qualifying pushed head. A new
  head replaces the old window. If no run appears, close silently. Once a run
  appears, track until every run for that head is terminal. A no-run gate costs
  at most 24 requests: one at time zero, then one every 5 s through 115 s.
- **Ceiling:** one GitHub request every 5 s across the server, or 720 request
  attempts per rolling hour. Repositories share this scheduler. Expanded job
  detail consumes the same request slots instead of starting another timer.
- **Caching:** retain the `ETag` per exact URL and send `If-None-Match` on the
  next GET. Stop on terminal state. Honor `Retry-After` and
  `X-RateLimit-Reset`; do not query `/rate_limit` merely to inspect quota.

The discovery URL is stable and specific:
`GET /repos/{owner}/{repo}/actions/runs?head_sha={sha}&per_page=100`.
GitHub documents `head_sha` filtering and `Actions: read` for this endpoint.
The original query also pinned `event=push`. That was dropped (2026-08-25):
`event` accepts one value per request, and a `pull_request` run carries the
PR's head commit as its `head_sha` — the same commit the local push wrote —
so filtering on `push` hid every branch whose workflows only trigger through
a pull request while costing an extra request to recover.
The jobs endpoint uses the same fine-grained permission. See the official
[workflow-run endpoint](https://docs.github.com/en/rest/actions/workflow-runs?apiVersion=2026-03-10#list-workflow-runs-for-a-repository)
and [workflow-job endpoint](https://docs.github.com/en/rest/actions/workflow-jobs?apiVersion=2026-03-10#list-jobs-for-a-workflow-run).

The 720 ceiling counts network attempts, not only rate-limit charges.
Authenticated conditional GETs that return `304 Not Modified` do not count
against the primary limit, but they remain HTTP traffic. GitHub gives normal
authenticated user requests a 5,000-per-hour primary limit and unauthenticated
requests a 60-per-hour IP limit. Sources: [conditional request guidance](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api?apiVersion=2022-11-28#use-conditional-requests-if-appropriate)
and [REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api?apiVersion=2022-11-28).

### Transport comparison

| Transport | Auth and permission | Requests and latency | Failure and offline behavior | Decision |
|---|---|---|---|---|
| Event-gated REST polling | `gh auth token --hostname github.com`; classic PAT/OAuth needs `repo` for private repos, fine-grained PAT needs repository `Actions: read`. Scribe does not use unauthenticated access even for public repos. | 0 idle. Immediate GET, then 5 s. Maximum 720 attempts/hour server-wide. `ETag` reduces primary-rate charges on unchanged responses. | A missed or unrecognized ref update misses the run. Fetch can produce a false gate, bounded to 120 s. Before a run is seen, offline/auth failure stays hidden. After observation, retain last state as stale and back off. | **Default.** It uses existing auth, no repo mutation, and no hosted receiver. |
| Opt-in `gh webhook forward` | This is the separately installed `cli/gh-webhook` extension, not a core `gh` command. Repository forwarding needs admin access and webhook creation rights. A fine-grained token needs repository `Webhooks: write`; organization forwarding needs `admin:org_hook`. | No REST polling while connected, but a long-lived WebSocket and webhook setup remain. Delivery is normally event-latency. | GitHub supports it only for testing and development. The command must keep running, only one user can forward a repo or organization at once, and no durable replay is promised after disconnection. | **Removed entirely** (user decision at the approval gate, 2026-08-14): not shipped, not a sanctioned tool, no opt-in path. |
| Continuous ETag polling | Same Actions read permission as event-gated polling. Correct authorization is required for the documented `304` primary-limit exemption. | At 5 s it makes 720 HTTP requests/hour while idle. A `304` may cost no primary quota, but it still uses network and GitHub documents no general request-free subscription. | Reconciles after transient failures, but offline mode keeps waking and retrying unless another gate stops it. Rate-limit responses require the documented delay. | **Reject as default or automatic fallback.** It violates zero idle traffic. |
| Standard repository, organization, or GitHub App webhook | Creating a repository webhook needs owner/admin access. Fine-grained tokens need `Webhooks: write`; a GitHub App subscribing to `workflow_run` needs `Actions: read`. | Genuine HTTP push, no polling. `workflow_run` has `requested`, `in_progress`, and `completed` activity types. | Needs a GitHub-reachable receiver or relay. Deliveries may be delayed or reordered. GitHub does not automatically redeliver failures, including receiver downtime. The receiver must verify signatures and answer promptly. | **Future hosted option only.** It conflicts with v1's no-hosted-service scope and asks for more privilege. |

Official sources: [`gh webhook forward`](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/using-the-github-cli-to-forward-webhooks-for-testing),
[the extension's forwarding source](https://github.com/cli/gh-webhook/blob/main/webhook/forward.go),
[repository webhook permissions](https://docs.github.com/en/rest/repos/webhooks?apiVersion=2026-03-10#create-a-repository-webhook),
[`workflow_run` payloads](https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_run),
[`workflow_run` activity types](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_run),
and [failed delivery behavior](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries).
The official [`gh run watch` manual](https://cli.github.com/manual/gh_run_watch)
confirms a 3 s refresh interval, so it is polling rather than an Actions
stream. No supported Actions SSE or general client WebSocket subscription was
found in the official REST, GraphQL, CLI, or webhook documentation. This last
sentence is an inference from the documented interfaces, not an API guarantee.

### `gh` degradation matrix

The server always passes `--hostname github.com`. It accepts `gh`'s active
account and environment-token precedence instead of creating another account
selector. See [`gh auth token`](https://cli.github.com/manual/gh_auth_token),
[`gh auth status`](https://cli.github.com/manual/gh_auth_status), and
[`gh` environment variables](https://cli.github.com/manual/gh_help_environment).

| Condition | Required behavior |
|---|---|
| `gh` absent | Executable lookup fails. Log once, hide the bar, and make no API request. Do not install anything or fall back to anonymous access. |
| `gh` present but unauthenticated | `gh auth token --hostname github.com` exits nonzero. Log once, hide the bar, and make no API request. Never prompt from the server. |
| Multiple hosts | The explicit hostname prevents a GHES account from being selected. GHES remains outside v1. |
| Multiple accounts on `github.com` | Without `--user`, `gh auth token` selects the active account. Use only that account. Never probe all stored users or call `gh auth switch`. If it cannot read the pushed repo, degrade as insufficient permission. |
| Fine-grained PAT | Direct workflow-run and workflow-job REST calls work only when the token includes the target repository and `Actions: read`. Do not use `gh run watch`; its manual says fine-grained PATs are unsupported because that command also needs Checks data. |
| Insufficient permission | GitHub may return `403`, or `404` for a hidden private resource. Before observation, hide and stop the window. During an observed run, mark stale. Do not retry every 5 s. |
| Revoked or expired token | `gh auth token` may still return a locally stored token. On the first API authentication failure, discard the in-memory token and stop the window. Keep an observed run stale. A later push gate can retry after the user fixes `gh` auth. |
| Offline or DNS/TLS failure | Before observation, keep the bar hidden and retry with bounded backoff until the 120 s discovery deadline. During an observed run, keep last-known state stale. Never block terminal work. |

GitHub says revoked or expired tokens can no longer authenticate API requests.
For private resources it may answer `404` when authentication or permission is
wrong. Sources: [token expiration and revocation](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/token-expiration-and-revocation)
and [REST troubleshooting](https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api?apiVersion=2022-11-28#404-not-found-for-an-existing-resource).

### Ref-state prototype

Temporary repositories under `/tmp` tested a named remote push, `pack-refs`, a
second push, and a linked worktree. Fixtures were deleted afterward; no project
repository state changed.

An actual `notify` 8.2 watcher saw loose-ref lock/create/write/rename bursts,
the `packed-refs.new` rename and loose-ref removals from `pack-refs`, and a
linked worktree's private `HEAD` plus shared branch-ref events. A local
commit-style `git update-ref` changed only `refs/heads/*`; the later successful
named-remote push separately updated `refs/remotes/origin/*`. Debouncing and
comparing logical snapshots cleanly separates those two operations. Repacking
caused events but no logical OID change, so it did not qualify as a push gate.

| Layout | Observed result | Watch rule |
|---|---|---|
| Loose refs | A successful `git push -u origin main` created `refs/remotes/origin/main` at the pushed OID. | Watch `$GIT_COMMON_DIR/refs` recursively. |
| Packed refs | `git pack-refs --all` removed the loose remote-tracking file while `git show-ref` still resolved it from `packed-refs`. The next successful push recreated a loose remote-tracking ref at the new OID. | Also watch `$GIT_COMMON_DIR` non-recursively so atomic `packed-refs` replacement triggers a rescan. Never parse the file. |
| Linked worktree | Its `.git` file pointed at `.git/worktrees/<id>`. `git rev-parse --absolute-git-dir` returned that private directory, while `git rev-parse --path-format=absolute --git-common-dir` and `--git-path refs/remotes/origin/main` returned the shared main `.git` paths. | Resolve paths through Git. Do not append `/HEAD` or `/refs` to the worktree's `.git` file path. |
| Reftable | Host Git is 2.43.0 and `git init -h` has no `--ref-format`, so it cannot create a real reftable fixture. Current Git documents binary tables under `$GIT_DIR/reftable/` and `tables.list` as the live stack. Direct loose/packed parsing therefore cannot support it. | Watch the resolved common `reftable` directory non-recursively, then ask Git for the snapshot. A Git binary that can open the repo also owns reftable decoding. |

Git explicitly says callers should use commands such as `git rev-parse` and
`git update-ref` instead of assuming paths inside `$GIT_DIR`. Its layout docs
also define gitfiles, shared `packed-refs`, and the reftable stack. Sources:
[worktree refs and path resolution](https://git-scm.com/docs/git-worktree#_refs),
[repository layout](https://git-scm.com/docs/gitrepository-layout), and
[reftable layout](https://git-scm.com/docs/reftable).

Implementation should use one canonical snapshot after any watch event:
`git for-each-ref` over the configured remote-tracking namespace, with OIDs and
ref names in a delimiter-safe format. A qualifying gate is a changed remote
tracking ref whose new OID equals a local branch tip and whose remote push URL
canonicalizes to `github.com/{owner}/{repo}`. The API query remains the source
of truth. Fetches may still cause harmless false gates. Pushes to a raw URL or
to a destination that has no configured remote-tracking mapping can leave no
local ref change and are a documented v1 miss.

No new watcher crate is needed. Workspace `Cargo.toml:53` already pins
`notify = "8"`, and `crates/scribe-client/Cargo.toml:27` already uses it.
`notify` 8.2 provides both a platform [`RecommendedWatcher`](https://docs.rs/notify/8.2.0/notify/type.RecommendedWatcher.html)
and a stdlib-based [`PollWatcher`](https://docs.rs/notify/8.2.0/notify/poll/struct.PollWatcher.html).
Use the native watcher first. If it cannot watch the resolved paths or reports
an overflow, fall back to `PollWatcher` on only the small ref paths at a 2 s
interval. That fallback performs local filesystem reads, not GitHub requests.

### Protocol compatibility

An unknown top-level MessagePack enum variant fails Serde decode. Framing does
consume the whole length-prefixed body first, so stream alignment survives
(`crates/scribe-common/src/framing.rs:13-41`). The established server loop
explicitly discards a client-frame `Deserialization` error and continues
(`crates/scribe-server/src/ipc_server.rs:6064-6083`). The client is different:
its reader propagates every `read_message::<ServerMessage>` error and exits
(`crates/scribe-client/src/main.rs:11561-11575`), after which the connection
supervisor reconnects (`crates/scribe-client/src/main.rs:10124`). Sending a new
CI `ServerMessage` to an N-1 local client could therefore cause a reconnect
loop. It does not safely discard only the new frame.

Remote and LAN transports already require an exact
`REMOTE_PROTOCOL_VERSION` match, and the source requires a bump for every
remote-visible semantic change (`crates/scribe-common/src/protocol.rs:15-38`).
The local Unix socket has no version gate because client and server normally
ship together, but hot upgrade creates a real N/N-1 overlap. Add a
default-false CI capability to `ClientMessage::Hello`, matching the existing
capability pattern at `crates/scribe-common/src/protocol.rs:407-433`, and emit
CI frames only to clients that advertised it. The existing backward-decode
test at `crates/scribe-common/src/protocol.rs:1599-1627` shows how a missing
Hello field defaults safely; implementation must add the equivalent CI test
and a test proving the server does not send CI frames without the capability.

The client-to-server dismiss variant needs no separate N-1 gate. An old server
already discards an unknown client frame and preserves the connection. A new
client should still treat dismiss as best effort because an old server will
not synchronize it. Bump `REMOTE_PROTOCOL_VERSION` for remote/LAN peers and
retain the Hello capability for local hot-upgrade safety.

### Recommended architecture and fallback

Keep the server ownership already chosen above. Resolve each repo through Git,
watch its ref storage with the already-installed `notify`, and open one shared
120 s polling window only when a remote-tracking OID changes to a local pushed
head. Fetch run rollup immediately and every 5 s through the server-wide
720-request/hour scheduler, using ETags. Fetch job detail only while expanded
and from the same scheduler. Advertise CI support in `Hello`; never send the
new server frame to an N-1 client.

Concrete fallback: if native filesystem notifications are unavailable or
overflow, replace only that repo's watcher with `notify::PollWatcher` at 2 s.
The transport stays push-gated REST, and idle GitHub request count stays zero.
If Git cannot resolve the repo or `gh` cannot authenticate, disable the tracker
for that repo with one diagnostic log entry. Do not fall back to continuous
ETag polling or a production `gh webhook forward` process.
