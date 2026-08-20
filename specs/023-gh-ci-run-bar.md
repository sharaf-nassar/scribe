# gh-ci-run-bar

## Problem Statement

When a developer pushes from a Scribe session to a GitHub repo with Actions
configured, CI feedback lives outside the terminal: they alt-tab to a browser
tab or run `gh run watch` by hand. Scribe already knows the workspace's repo
and branch. It should surface an in-progress GitHub Actions run as a bar at
the top of the workspace so the user can see and track the run in realtime
without leaving the terminal, and get notified when a run starts.

The approved research and mockup now ship as a server-owned tracker, additive
IPC, and collapsed plus expanded GPUI trace. This spec records the as-built v1
contract and separates offline automation from the required real-GitHub gate.

## Goals

- A workspace whose repo is hosted on github.com shows a CI bar at the top
  of the workspace for the user's last locally pushed head commit. All
  observed workflows contribute to a worst-status rollup. Terminal state
  remains visible until the owner dismisses it or a later observed head
  replaces it.
- The bar tracks the run in near-realtime: overall status, per-job progress,
  elapsed time, and completion result (success/failure/cancelled).
- The user is notified when their own push triggers a CI run without Scribe
  constantly polling GitHub — steady-state network traffic when no run is
  active is zero.
- Research deliverable: a written comparison of trigger-detection and
  tracking transports (event-gated polling, `gh webhook forward`, conditional
  requests / ETag polling, others found) with a recommendation.
- Mockup deliverable: a visual mockup of the bar (states: triggered, running
  with job progress, success, failure, expanded detail) consistent with
  Scribe's existing chrome, usable as the reference during implementation.
- Auth reuses the user's existing `gh` CLI credentials; no new token
  management UI.

## Non-Goals

- Non-GitHub CI providers (GitLab CI, Buildkite, Jenkins, …). The detection
  seam should not preclude them, but only GitHub Actions is in scope.
- Runs not triggered by the user's own local push: pushes from other
  machines, teammates' merges, scheduled/dispatch workflows. Documented
  limitation of v1, not a bug.
- Pushes made inside remote (SSH) sessions — they mutate the remote host's
  repo and are invisible to local detection. v1 covers local repos only.
- GitHub Enterprise Server / non-github.com hosts. The host seam stays
  generic but only github.com is supported in v1.
- PR check rollups (check-runs API, third-party checks). v1 tracks the
  workflow runs for the pushed head commit.
- Acting on CI from the bar (re-run, cancel, approve deployments) — v1 is
  view/track only, with at most "open in browser".
- Full log streaming of job output inside the bar. Realtime here means
  status/job/step progression, not a log viewer.
- Historical run browsing / a runs list UI. Only the current (most recent
  relevant) run is surfaced.
- Hosting any Scribe-side web service or webhook receiver infrastructure.
- Webhooks in any form — repo/org webhook creation and `gh webhook
  forward` alike. Removed entirely at the approval gate (2026-08-14):
  GitHub documents forwarding as dev/test-only, and webhook transports
  need privileges and infrastructure v1 refuses.
- Desktop OS notifications; "notified" means the bar appearing (and any
  existing in-app notification surface), not system toasts, unless research
  finds Scribe already has a fitting surface.

## Backlog Inputs

None. No source P4 beads; this is a fresh feature from a user prompt.

## Target Epic

No epic exists; this run will create one.

## User Stories

### Story 1 — Research: transport and trigger detection

As a Scribe maintainer, I want a researched recommendation for how Scribe
learns that a CI run started and how it tracks progress, so that the
implementation avoids constant polling and stays within GitHub rate limits.

Acceptance criteria:

- Compares at least: (a) event-gated polling — Scribe detects the local
  `git push` (session command or `.git` refs change) and polls only during
  an active window; (b) `gh webhook forward` / gh CLI webhook forwarding;
  (c) ETag/conditional-request polling and current GitHub rate-limit
  semantics; (d) any push/streaming option discovered.
- Each option is verified against current GitHub docs (not training data),
  noting auth requirements, repo permissions needed, failure modes, and
  behavior when the machine is offline (constitution #6).
- States expected request volume in steady state and during an active run.
- Names which component owns detection/tracking (server vs client) with
  rationale against crate boundaries (constitution #1).
- Ends with one recommended architecture and a fallback.

### Story 2 — Research: UX prior art and bar design

As a Scribe maintainer, I want a survey of how existing tools surface CI
status in-editor, so the bar's design is deliberate rather than improvised.

Acceptance criteria:

- Surveys at least 3 prior-art surfaces (e.g. VS Code GitHub Actions
  extension, GitHub Desktop's checks display, `gh run watch` output).
- Defines the bar's states: hidden/idle, run triggered, running (with
  job/step progress), success, failure, cancelled; and what "dismiss" does.
- Defines where the bar lives relative to Scribe's existing chrome
  (titlebar/tab bar, per-pane prompt bar, bottom status bar) and how it
  behaves with multiple workspaces and multiple windows.
- Refines the decided run rule (all workflow runs for the user's last
  pushed head commit, aggregated worst-status) into written edge-case
  rules: force push, rapid successive pushes, manual re-runs of a tracked
  run, and a push that triggers no workflows.

### Story 3 — Mockup

As a Scribe maintainer, I want a visual mockup of the CI bar in its key
states, so implementation later has a concrete reference.

Acceptance criteria:

- Mockup shows: running state with per-job progress, success, failure, and
  the expanded detail view (if the design has one).
- Visual language matches Scribe's existing theme system (dark-first,
  workspace badge colors respected) rather than a generic web aesthetic.
- Reviewable as a file in the repo (format decided by research — e.g. HTML
  mock, SVG, or annotated screenshot) and referenced from the spec.
- User has seen and approved the mockup direction.

Delivered and APPROVED (user, 2026-08-14):
`specs/023-gh-ci-run-bar/mockup/ci-bar.html` — the trace direction — is
the binding visual reference for implementation; implement exactly that
(recorded on beads scribe-gygu.10/.11/.14 and the epic). Design rules and
research live in `specs/023-gh-ci-run-bar/research.md`. The transport
recommendation's approval is tracked at the scribe-gygu.5 gate.

### Story 4 — CI bar in the workspace

As a Scribe user, I want an in-workspace bar tracking my repo's active CI
run, so I don't need a browser tab to know whether my push is green.

Acceptance criteria:

- With a github.com-hosted repo and workflow runs in progress for the
  user's last local push, the bar appears at the top of the workspace
  within the latency bound chosen by research, and updates through to
  completion, aggregating all triggered workflows (worst-status rollup).
- With no active run, no github.com remote, or no `gh` auth, the bar is
  absent and Scribe generates zero recurring GitHub traffic.
- The feature is a global setting, off by default (constitution #6). Once
  enabled, missing prerequisites (`gh` absent, unauthenticated) degrade
  silently with a logged reason.
- Shared-session viewers (LAN share, remote window control) see the bar
  read-only; "open in browser" acts only on the owning user's client.
- Failure of the GitHub integration during an active tracked run shows a
  quiet stale indicator; outside an active run the bar is simply absent —
  never a blocking error, never terminal disruption.

## Constraints

- Avoid constant polling: steady-state (no active run) GitHub traffic must
  be zero; polling is bounded to push-gated active-run windows.
- Transport is push-gated conditional REST polling per the approved
  research (zero idle requests, 5 s in-window cadence, 120 s discovery
  window, ETag caching). Webhooks are out of scope entirely — see
  Non-Goals.
- The implementation follows the approved research and trace mockup. The
  completed approval record remains in the research artifact and Story 3.
- Local-first (constitution #6): all network access is optional and gated;
  the feature must never transmit terminal contents; offline Scribe works
  unchanged.
- Reuse existing seams (constitution #1): server already walks `.git/HEAD`
  per session for branch detection; workspaces are configured root dirs with
  badge colors; the GPUI client owns titlebar/tab bar, per-pane prompt bar,
  and bottom status bar. New UI must fit that chrome (constitution #2).
- Auth via the user's existing `gh` CLI login (`gh auth token`) rather than
  a Scribe-managed OAuth flow; absence of `gh` auth disables the feature
  silently.
- External API claims must be verified against current GitHub docs during
  research (constitution #7); rate-limit and conditional-request semantics
  change over time.
- Performance (constitution #4): the bar must not affect terminal latency or
  frame stability; CI tracking runs off the render path.

## Open Questions

All resolved by the completed research (`specs/023-gh-ci-run-bar/research.md`)
and the approval gate:

- Trigger detection — validated by a watcher prototype across loose refs,
  packed-refs, worktree indirection, with a documented reftable rule
  ("Ref-state prototype").
- `gh webhook forward` — verified dev/test-only; webhooks removed entirely
  at the gate (Clarifications Q4, revised).
- ETag/304 semantics — verified: authenticated conditional 304s don't
  count against the primary limit but remain HTTP traffic; the 720/hour
  ceiling counts attempts ("Numeric contract").
- Bar placement — workspace-region chrome band ("Placement rules").
- Ownership — server-owned, validated against remote attach and hot
  upgrade ("Ownership conclusion").
- Notification surface — the bar's appearance is the v1 notification
  ("Notification surface decision").

## Clarifications

**Q1: How does the feature turn on?**
A: Global setting, off by default. Once enabled, missing prerequisites
(`gh` absent or unauthenticated) degrade silently with a logged reason.
Reflected in Story 4 acceptance criteria.

**Q2: Which runs does v1 promise?**
A: Only runs triggered by the user's own local push. Pushes from other
machines, teammates' merges, scheduled workflows, and pushes inside remote
(SSH) sessions are explicit v1 non-goals. Reflected in Goals and Non-Goals.

**Q3: Branch workflow runs or PR check rollups; one workflow or all?**
A: All workflow runs for the pushed head commit, aggregated into a
worst-status rollup (like GitHub's checks UI). PR check rollups are a v1
non-goal. Reflected in Goals, Non-Goals, and Stories 2 and 4.

**Q4: May Scribe create a webhook on the user's repo?**
A: Not by default — but allowed as an explicit opt-in enhancement. Research
must treat webhook forwarding as an eligible opt-in transport and document
its permissions and support status. Reflected in Constraints.
REVISED at the approval gate (2026-08-14): after research showed
`gh webhook forward` is a dev/test-only extension and webhook transports
need admin privileges or hosted infrastructure, the user removed webhooks
entirely — no opt-in path remains. Reflected in Non-Goals and Constraints.

**Q5: Do shared-session viewers see the bar?**
A: Yes, read-only. The bar renders in shared surfaces (LAN share, remote
window control) but "open in browser" acts only on the owning user's
client, and no token or GitHub credential ever flows to viewers.
Reflected in Story 4 acceptance criteria.

**Q6: GHES / enterprise hosts in v1?**
A: No — github.com only. The host seam stays generic; GHES is a declared
non-goal. Reflected in Non-Goals.

**Q7: Tracking UI depth?**
A: v1 ships both the collapsed one-line bar (status, workflow names,
elapsed) and the expandable per-job/step detail panel. Job requests exist
only while at least one matching panel is open.

## Architecture Approach

The server-owned CI tracker detects the user's local push or trusted same-OID
generation by watching workspace repos' ref state, then opens a bounded polling
window against the GitHub Actions API using the user's `gh` credentials,
aggregates all workflow runs for the pushed head commit, and broadcasts CI
state to attached clients over new IPC messages. The GPUI client renders the state as a
dynamic chrome band at the top of the workspace, following the per-pane
prompt bar precedent (bands appear/disappear with grid re-measure).

The completed research and approved mockup remain the rationale and visual
reference. The implementation inventory below records the resulting modules
and exact bounds.

Token handling: the tracker obtains the token via `gh auth token` at need. It
never crosses client/server IPC or LAN/remote transports. `HttpGithubApi`
sends it only to fixed `https://api.github.com` or an explicit loopback test
override, never to a host from an untrusted remote URL.

Performance: all tracking runs server-side, entirely off the client
render path; bar updates arrive as bounded IPC deltas so terminal
latency and frame stability are unaffected (constitution #4).

Alternatives rejected:
- Client-owned polling — duplicates traffic per attached client and
  breaks under remote attach, where the repo lives on the server machine
  (constitution #1/#2).
- Webhook transport — requires webhook-creation permission or hosted
  infrastructure and was removed from v1 at the approval gate.
- Continuous background polling — violates the zero steady-state traffic
  constraint (Clarification Q2, constitution #6).
- PTY command-text push detection — untrusted input must never initiate
  network requests (constitution #5); at most a supplementary hint.

## Affected Components

- `crates/scribe-server` — CI tracker module: ref-state push
  detection (building on the existing per-session `.git/HEAD` walk seam
  for repo discovery), push-gated polling windows, run aggregation.
  Reuses the updater's reqwest seam and its API-base-URL override
  pattern (`updater.rs`, `SCRIBE_UPDATE_API_URL` precedent). One tracker
  per (host, repo) shared across workspaces/windows/clients. Every
  silent-disable path logs a diagnosable reason.
- `crates/scribe-common` — `protocol.rs`: server→client CI state
  message and a client dismiss message; config schema: global off-by-
  default toggle.
- `crates/scribe-client` (GPUI) — workspace-top chrome band (collapsed
  bar + expandable detail panel), themed, with non-color status
  signifiers; renders read-only for shared viewers. Hosts the
  "open in browser" affordance (run URL derived from repo + run id),
  gated to act only on the owning user's client.
- `specs/023-gh-ci-run-bar/` — research artifact (`research.md`) and
  mockup files (`mockup/`), the approval gate's inputs.
- Integrated settings window — the enable toggle, live-applied like other
  config keys.
- `tests/e2e/` + docker harness — fake GitHub Actions API on 127.0.0.1
  inside the `--network none` container; func + visual suites.
- `lat.md/` — tracker, protocol, bar UI, and test contracts.

## Data Model

In-memory server state only; no persistence, no migrations; state is
cheaply re-derivable after hot upgrade (constitution #2/#7).

- `GithubRepository`: validated owner and name resolved from the push-target
  github.com remote.
- `CiRunState`: trusted `owner/name`, head SHA, branch, at most 100 workflow
  entries (run id, name, status, conclusion, observation timestamps),
  worst-status rollup, and stale flag. Elapsed text is derived client-side.
- `CiRunDetails`: head-qualified jobs and steps, fetched only while a matching
  panel is open. Each workflow response is bounded to 100 jobs and each job
  to 100 steps; provider strings are truncated to 256 UTF-8 bytes.
- Dismissal is server state keyed by repository root and head. It is not part
  of `CiRunState` and no credential or token enters either message.
- Config: `github_ci.enabled`, default false.

The server tracker records each workflow's first and latest local observation
times. This gives the bar a stable elapsed clock without adding a provider-date
parser; a re-observed run keeps its first timestamp by run id.

The tracker normalizes each poll response to the newest run per workflow at
the pushed head, so a superseded run (a retag replacing an earlier attempt)
never enters rollup, details, link selection, or terminal-stop decisions,
while distinct workflows running concurrently at the same head both survive.
A trusted same-OID ref event reopens an active window at an unchanged head in
place, carrying its observed state and roots forward rather than clearing it;
only an actual head change clears and opens a fresh window.

Hot handoff carries active repository/head descriptors, remaining discovery
time, roots, and last bounded run state. It never carries the GitHub token. The
successor re-polls each descriptor before returning to the 5 s cadence.

`github_ci.enabled` changes tracking eligibility live. Saving or reloading the
setting does not invoke `gh`, test authentication, or make an HTTP request;
those checks begin only after a later qualifying local-push gate.

## API / Interface Changes

- `Hello.ci_run_bar` defaults false for older local clients and gates both
  `CiRunState` and `CiRunDetails` server frames. Remote protocol version 6
  carries the CI messages under the existing exact-version handshake.
- `CiRunState { repo_root, delta }` sends a full replacement or
  head-qualified clear. `CiRunDetails { repo_root, details }` goes only to
  interested capable writers.
- `DismissCiRun` synchronizes owner dismissal. `SetCiRunDetailsInterest`
  carries root, head, and open state; capable read-only viewers may request
  details but cannot invoke host actions.
- `SCRIBE_GITHUB_API_URL` is a loopback-only test seam that mirrors
  `SCRIBE_UPDATE_API_URL`.
- No CLI surface changes.

## Automated verification

Automated coverage is deliberately offline. Every runtime check uses the
Docker E2E images with `--network none`; none can validate github.com itself.

- Server, protocol, and client unit tests cover configuration, logical ref
  changes, URL trust, request bounds, aggregation, handoff, IPC authorization,
  state models, region geometry, accessibility, and owner-only actions.
- `tests/e2e/github-actions-api.sh` checks the loopback fake API in both Docker
  images, including head filtering, run and job progression, ETags, `304`,
  rejected routes, and the JSONL request log.
- `tests/e2e/func/ci-run-bar.sh` uses a real local push and the fake API to
  assert zero requests while disabled and idle, first state within 10 seconds,
  5-second refreshes, queued/running/success progression, ETag reuse, stale
  fallback, and identical read-only viewer state. It also re-creates a loose
  tag at the same tracked OID after failed run 104, keeps polling through an
  old-only response, and proves run 105 alone owns the new state and URL.
- `tests/e2e/func/ci-run-details.sh` proves job requests are absent while the
  panel is closed, start after open interest, and stop after close interest.
- `tests/e2e/visual/ci-run-bar.sh` checks the 40px collapsed reflow, terminal
  frame stability, theme pixels, and non-color cues for running, passed,
  failed, cancelled, and stale states.
- `tests/e2e/visual/ci-run-details.sh` checks pointer and keyboard toggles,
  both interest messages, terminal input isolation, and the expanded trace.

These checks do not prove installed `gh` authentication, current github.com
REST behavior, GitHub-side run creation latency, or the browser destination.

## Maintainer manual verification

This is a release gate. A Scribe maintainer with write access must run it on an
installed release-candidate build against a real github.com repository. A
loopback fixture, fork without write access, or Docker run does not satisfy it.

Use a repository with a harmless disposable branch and at least one Actions
workflow triggered by `push`. Do not record or print the authentication token.

| Step | Action | Expected result |
| --- | --- | --- |
| 1. Authenticate | Run `gh auth status --hostname github.com`. | The command exits successfully and names the active github.com account that can read Actions for the repository. |
| 2. Enable | In Scribe Settings, enable Updates > GitHub CI run status. Open a local session whose current directory is inside the repository. | No restart occurs. No CI bar appears before a new local push. This visual check does not prove zero idle HTTP traffic; Docker request logs cover that contract. |
| 3. Push | Make a normal harmless commit on the disposable branch, then run `git push origin HEAD` in that Scribe session. Record push completion, GitHub run visibility, and bar appearance times. | GitHub creates run entries for the pushed head. The collapsed bar appears within 10 seconds after those runs become visible and shows the branch plus the first seven head-SHA characters. |
| 4. Compare | Compare the collapsed glyph, state word, workflow names, completed count, and elapsed time with the repository Actions page. | Scribe matches GitHub's queued/running state. The band occupies only the matching workspace region and keeps a 40px collapsed height. |
| 5. Expand | Click the band, then close and reopen it with Enter or Space while its toggle has focus. | A loading row gives way to the real jobs and current steps on a shared minute axis. Closing returns the terminal rows and does not type Enter or Space into the terminal. |
| 6. Finish and open | Let the run reach any terminal conclusion, then use Open CI run. | Passed, failed, or cancelled matches GitHub and stops repeating motion. The browser opens `https://github.com/{owner}/{repo}/actions/runs/{run_id}` for the preferred visible workflow. |
| 7. Dismiss | Use Dismiss CI run, then restore the repository and setting to their prior state through normal maintainer cleanup. | The matching head disappears from every local capable view. No credential appears in Scribe logs or captured evidence. |

Retain the release-candidate commit, repository and head SHA, run URL, the three
timestamps from step 3, and collapsed, expanded, and terminal screenshots.
Record a failure if any expected result differs; do not replace real-GitHub
evidence with an offline rerun.

## Known limits

- A synthetic fixture checks the reftable watch path because the audited host
  Git cannot create a real reftable repository. Loose refs, packed refs, and
  linked worktrees use real repositories.
- A remote-tracking OID change or an exact mutating event for a loose tracked
  ref can open a window. A loose tag at an OID already shared by a local branch
  and tracked remote may also reopen that OID. Tags at untracked OIDs cannot
  infer a destination. Access events and packed or reftable storage rewrites
  remain non-triggers.
- Remote SSH pushes, scheduled runs, teammate pushes, manual re-runs without a
  local ref event, and no-op pushes that write no ref remain invisible.
- Terminal snapshots stop polling but stay visible in the client until owner
  dismissal or replacement by an observed later head.

## Historical implementation sequence

All phases below are complete. This record explains dependency order; it is
not a list of remaining work.

Phase A: research
- Ownership conclusion (Story 1 milestone, early): validate the
  server-owned lean against remote-attach topology and hot-upgrade
  continuity. Split out as its own deliverable so UX research unblocks
  without waiting for the full transport report. Blocks: UX research
  finalization.
- Transport & trigger research (Story 1, remainder): verify ETag
  semantics, webhook forwarding status/permissions, `gh` degrade matrix
  (multi-account, PAT scopes, revocation), any push/streaming option
  discovered, offline behavior per option; verify old-client
  frame-discard behavior and whether N/N-1 protocol gating is required
  for the new messages; prototype ref-state watch across ref backends;
  finalize numeric bounds (latency, poll interval, window timeout,
  requests/hour ceiling). Artifact:
  `specs/023-gh-ci-run-bar/research.md`. Blocks: approval gate.
- UX & prior-art research (Story 2): survey ≥3 prior-art surfaces; write
  the bar-state machine and placement rules (multi-window, splits,
  shared-viewer rendering); finalize edge-case run rules; decide whether
  an existing in-app notification surface should also announce the
  run-triggered moment. Depends on the ownership conclusion. Findings
  land in the same research artifact. Blocks: mockup, approval gate.

Phase B: mockup and approval
- Mockup (Story 3): static themed HTML/SVG under
  `specs/023-gh-ci-run-bar/mockup/`, all states including
  stale/degraded, non-color signifiers, collapsed + expanded views.
  Depends on UX research. Blocks: approval gate.
- Approval gate: user approves research recommendation + mockup
  direction. Blocks every implementation task.

Phase C: implementation:
- Config toggle + settings UI surface.
- Server: ref-state push detection.
- Server: GitHub client, push-gated polling window, aggregation, logged
  disable reasons (depends on push detection; uses config toggle).
- Protocol: CI state + dismiss messages, client state plumbing (depends
  only on the CiRunState definition; parallel with the GitHub-client
  work, converging at integration).
- Client: collapsed bar band, including the owner-gated "open in
  browser" affordance and read-only shared-viewer rendering (depends on
  protocol plumbing + mockup).
- Client: expandable per-job detail panel — in-scope task, P3: per-job
  data fetched only while the panel is open; ships after the bar-first
  phase (depends on collapsed bar).
- E2E infra: fake GitHub API fixture binary + staging into
  `docker/Dockerfile.func` / `docker/Dockerfile.visual` + justfile
  plumbing (depends only on the approval gate and the env-override
  seam; parallel with the server tracker).
- E2E func suite (depends on tracker + bar + fixture).
- E2E visual suite (depends on bar + fixture).
- Docs close-out: final lat.md consolidation + `lat check` pass, spec
  sync, and the documented maintainer-run manual verification protocol
  against a real github.com repo (final, depends on all). Each
  implementation bead still updates its own lat.md sections per repo
  rules; this item is the consolidation, not the only doc work.

## Historical backlog refinement

None — this feature has no backlog inputs; no P4 sources exist to
disposition.

## Alignment fixes applied

- Token containment restated in Architecture Approach (must-fix, A).
- "Open in browser" affordance + owner-only gating added to Affected
  Components and the collapsed-bar work item (must-fix, A+B).
- Protocol compat verification (frame-discard, N/N-1 gating) assigned to
  the Story 1 research item (must-fix, B).
- E2E infra split out: fixture + Docker staging is its own item,
  parallel with the server tracker instead of serialized behind
  tracker + bar; func and visual suites are separate items (must-fix, B).
- Manual verification protocol doc owned by the docs close-out item
  (must-fix, B).
- Detail panel resolved to an in-scope P3 task with concrete ACs
  (per-job fetch only while panel open) (must-fix, B).
- Logged-reason degradation added to server component + testing
  (should-fix, A).
- Research/mockup artifact paths pinned to `specs/023-gh-ci-run-bar/`
  (should-fix, A+B).
- Story 1 scope extended with push/streaming discovery + offline
  behavior per option (should-fix, A).
- Notification-surface open question assigned to Story 2 (should-fix,
  A+B).
- Ownership conclusion split out as an early Story 1 milestone so UX
  research unblocks sooner (should-fix, B).
- Protocol item dependency loosened to the CiRunState definition
  (should-fix, B).
- lat.md restated as per-bead updates + final consolidation (should-fix,
  B).
- Reuse of the existing per-session `.git/HEAD` walk seam named in the
  server component; off-render-path perf note added to Architecture
  Approach with a frame-stability check in testing (should-fix, A).

## Historical spec review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders) were synthesized into the lists below. Cross-dimension
convergence noted per question.

### Critical Questions (answer before planning)

1. How does the feature turn on? The draft contradicts itself: "absence of
   `gh` auth disables silently" implies presence silently enables network
   traffic, while Story 4 says "off-by-default or gated". Constitution #6
   makes network access explicit opt-in. Flagged by: requirements,
   ambiguity, scope, stakeholders (4/6 dimensions).
2. Which runs does v1 promise? Zero steady-state traffic is only achievable
   for runs triggered by the user's own local push; runs from other
   machines, teammates' merges, and scheduled workflows are undetectable
   without background polling. Must be narrowed explicitly or the headline
   constraint is unsatisfiable. Remote (SSH) sessions push on the remote
   host and are invisible to local detection — in or out for v1? Flagged
   by: feasibility, scope, requirements.
3. Branch workflow runs or PR check rollups — and one workflow or all? A
   push routinely triggers several workflows, and most GitHub users consume
   CI as PR checks. The choice changes the API data model, the bar's
   semantics, and the mockup states. Flagged by: scope, ambiguity,
   requirements.
4. May Scribe create a webhook on the user's repo (the `gh webhook
   forward` transport needs webhook-creation permission and mutates repo
   config visible to all admins)? This policy bound decides which Story 1
   recommendation is even eligible. Flagged by: requirements, ambiguity,
   stakeholders, feasibility.
5. Does the CI bar render for shared-session viewers (LAN share, remote
   window control), exposing private repo names and CI state — and can a
   viewer trigger "open in browser" (a host action, constitution #5)?
   Flagged by: stakeholders.
6. Is v1 github.com-only, or must GHES / SSO-enforced orgs work? `gh`
   supports multiple hosts; scope changes detection, auth, and failure
   handling. Flagged by: gaps, scope, stakeholders.
7. How deep is v1's tracking UI: a one-line bar (overall status + elapsed +
   result) or the bar plus an expandable per-job/step detail panel?
   Per-job tracking multiplies API calls, mockup states, and GPUI work.
   Flagged by: scope, requirements.

### Technical Decisions (self-resolved — veto at the gate to override)

- Story 4 verification: fake GitHub API on 127.0.0.1 inside the
  `--network none` E2E container, enabled by an overridable API base URL —
  follows the existing `SCRIBE_UPDATE_API_URL` precedent in
  `crates/scribe-server/src/updater.rs` (constitution #3; designed in from
  the start, not retrofitted).
- Trigger detection: watch local `.git` ref state, not PTY command text —
  PTY output is untrusted and must never initiate network requests
  (constitution #5); command parsing is at most a supplementary hint.
  Research must cover loose refs, `packed-refs`, reftable (git ≥ 2.45),
  and `.git`-file worktree indirection.
- Token containment: the component making GitHub requests obtains the
  token via `gh auth token` at need; the token never crosses the
  client/server IPC or remote/LAN transports, and is only ever sent to the
  API host `gh` authenticated — never to a host derived from an untrusted
  remote URL (constitution #5/#6).
- Ownership lean: server-owned tracking (sessions are long-lived and
  server-owned; under remote attach the repo and refs live on the server
  machine). Story 1 must analyze the remote-attach topology and hot-upgrade
  behavior before locking this in.
- Provisional numeric bounds (research refines, plan finalizes): steady
  state = 0 requests; a push-gated active window opens on detected push and
  closes at run completion or after ~2 min if no run appears; in-window
  polling interval ≥ 5 s per repo; bar appears within ~10 s of run start.
- Steady-state wording conflict resolved strict: zero recurring GitHub
  requests when no active window (Goals' "near-zero" tightened to match
  Story 4).
- One tracker per (host, repo) per server, shared across workspaces,
  windows, and attached clients; dismissal syncs across views.
- Mockup medium: static themed HTML/SVG committed to the repo; no GPUI
  spike before direction approval.
- Mockup state list extended to include the stale/degraded state, and all
  states must use non-color signifiers (glyphs/text) alongside color.
- Degraded mode resolved: during an active tracked run, failures show a
  quiet stale indicator; outside an active run the bar is simply absent.
  Every silent-disable path logs a reason so "why no bar" is diagnosable.
- "Open in browser" is committed as v1's single interactive affordance.
- Research artifact (Story 1 + 2 output) lives in the repo alongside this
  spec and requires user approval before implementation begins — same
  approval condition as the mockup.
- Repo resolution rule: track the repo that actually received the push
  (the push-target remote), covering fork triangles; the exact
  multi-remote rule is a written Story 2 deliverable.
- Upgrade continuity: tracking state must be cheaply re-derivable after
  the server hot-upgrade handoff; any new IPC messages follow N/N-1
  protocol compatibility (constitution #2/#7).

### Non-Blocking Observations

- Story 1 degrade-matrix checklist: `gh` not installed, multi-account
  hosts (`gh` ≥ 2.40 account switching), fine-grained PATs lacking
  `actions: read`, token revoked mid-run.
- ETag/304 rate-limit exemption and `gh webhook forward` support status
  and permissions must be verified against live GitHub docs during
  Story 1 — both have shifted historically; do not assume.
- Story 2's placement/multi-window decisions depend on Story 1's ownership
  answer; sequence Story 1's ownership conclusion first.
- If gating (Q1) lands on a global toggle, revisit per-workspace
  enablement post-v1; retrofitting touches the settings schema.
- Dynamic chrome bands already exist in the GPUI client (per-pane prompt
  strip appears/disappears and the grid re-measures), so the bar's layout
  seam is real; placement is a design decision, not a capability gap.
