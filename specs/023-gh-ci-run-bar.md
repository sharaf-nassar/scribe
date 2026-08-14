# gh-ci-run-bar

## Problem Statement

When a developer pushes from a Scribe session to a GitHub repo with Actions
configured, CI feedback lives outside the terminal: they alt-tab to a browser
tab or run `gh run watch` by hand. Scribe already knows the workspace's repo
and branch. It should surface an in-progress GitHub Actions run as a bar at
the top of the workspace so the user can see and track the run in realtime
without leaving the terminal, and get notified when a run starts.

This run's immediate deliverables are (a) research into the optimal UX and
notification/transport mechanism and (b) a visual mockup, both to be consumed
by a later implementation pass. Implementation tasks are specced but expected
to be sequenced after research + mockup are approved.

## Goals

- A workspace whose repo is hosted on github.com shows a CI bar at the top
  of the workspace while GitHub Actions runs for the user's last locally
  pushed head commit are in progress — all triggered workflows aggregated
  into a worst-status rollup; the bar disappears (or collapses to a
  terminal state) when no tracked run is active.
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

### Story 4 — Implementation: CI bar in the workspace (sequenced later)

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
- Webhook creation on the user's repo is never the default transport, but
  is allowed as an explicit opt-in enhancement (e.g. `gh webhook forward`);
  research evaluates it as an eligible opt-in path.
- Research first: Stories 1–3 are the immediate deliverables; Story 4 is
  specced but sequenced behind mockup approval.
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

- Trigger detection: is watching `.git` ref state reliable enough as the
  "CI probably started" signal across ref backends (loose, packed-refs,
  reftable) and worktree layouts? (Research Story 1.)
- Does `gh webhook forward` still exist/work as a supported gh capability,
  and what permissions does it need? (Policy decided: opt-in only; research
  verifies mechanics and support status.)
- Do GitHub conditional requests (ETag → 304) still not count against the
  REST rate limit? This materially changes the polling budget.
- What does "top of a workspace" mean precisely in Scribe's GPUI layout —
  a bar under the titlebar spanning the window, or per-workspace within a
  split layout? How does it interact with the per-pane prompt bar?
- Server-owned or client-owned? Lean is server-owned (see Spec Review
  technical decisions); Story 1 validates against remote-attach topology
  and hot-upgrade continuity before lock.
- Is there an existing in-app notification surface the "run triggered"
  moment should also use, or is the bar's appearance sufficient?

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

**Q5: Do shared-session viewers see the bar?**
A: Yes, read-only. The bar renders in shared surfaces (LAN share, remote
window control) but "open in browser" acts only on the owning user's
client, and no token or GitHub credential ever flows to viewers.
Reflected in Story 4 acceptance criteria.

**Q6: GHES / enterprise hosts in v1?**
A: No — github.com only. The host seam stays generic; GHES is a declared
non-goal. Reflected in Non-Goals.

**Q7: Tracking UI depth?**
A: The mockup designs both the collapsed one-line bar (status, workflow
names, elapsed) and an expandable per-job/step detail panel;
implementation may phase bar-first. Reflected in Story 3.

## Architecture Approach

Server-owned CI tracker (research-validated before lock): a new
server-side module detects the user's local push by watching workspace
repos' ref state, opens a bounded polling window against the GitHub
Actions API using the user's `gh` credentials, aggregates all workflow
runs for the pushed head commit, and broadcasts CI state to attached
clients over new IPC messages. The GPUI client renders the state as a
dynamic chrome band at the top of the workspace, following the per-pane
prompt bar precedent (bands appear/disappear with grid re-measure).

This is a research-first plan: Phase A (research + mockup) precedes and
may amend the implementation phases; the analyze gate and the approval
gate both sit before implementation starts.

Token handling: the tracker obtains the token via `gh auth token` at
need; it never crosses the client/server IPC or the LAN/remote
transports, and is only ever sent to the API host `gh` authenticated —
never to a host derived from an untrusted remote URL (constitution
#5/#6, Clarification Q5).

Performance: all tracking runs server-side, entirely off the client
render path; bar updates arrive as bounded IPC deltas so terminal
latency and frame stability are unaffected (constitution #4).

Alternatives rejected:
- Client-owned polling — duplicates traffic per attached client and
  breaks under remote attach, where the repo lives on the server machine
  (constitution #1/#2).
- Webhook transport as default — requires webhook-creation permission and
  mutates repo config; allowed only as explicit opt-in (Clarification Q4).
- Continuous background polling — violates the zero steady-state traffic
  constraint (Clarification Q2, constitution #6).
- PTY command-text push detection — untrusted input must never initiate
  network requests (constitution #5); at most a supplementary hint.

## Affected Components

- `crates/scribe-server` — new CI tracker module: ref-state push
  detection (building on the existing per-session `.git/HEAD` walk seam
  for repo discovery), push-gated polling windows, run aggregation.
  Reuses the updater's reqwest seam and its API-base-URL override
  pattern (`updater.rs`, `SCRIBE_UPDATE_API_URL` precedent). One tracker
  per (host, repo) shared across workspaces/windows/clients. Every
  silent-disable path logs a diagnosable reason.
- `crates/scribe-common` — `protocol.rs`: new server→client CI state
  message and a client dismiss message; config schema: global off-by-
  default toggle.
- `crates/scribe-client` (GPUI) — new workspace-top chrome band (collapsed
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
- `lat.md/` — new sections for tracker, protocol messages, bar UI, tests.

## Data Model

In-memory server state only; no persistence, no migrations; state is
cheaply re-derivable after hot upgrade (constitution #2/#7).

- `CiRepo`: host (github.com only in v1), owner, name — resolved from the
  push-target remote.
- `CiRunState`: head SHA, per-workflow entries (run id, name, status,
  conclusion, timestamps), aggregated worst-status rollup, elapsed,
  stale flag, dismissed flag (syncs across clients).
- Per-job/step detail (expanded panel): fetched only while a window is
  active and the panel is open, if phased in.
- Config: one new boolean key, default false (exact key name follows the
  existing config naming conventions at implementation time).

## API / Interface Changes

- New server→client IPC message carrying `CiRunState` deltas, broadcast
  to clients attached to windows rooted in the affected repo. Additive
  protocol change; version/compat handling must follow the framing
  policy (a MessagePack decode failure discards only that frame) and the
  existing N/N-1 protocol compatibility conventions — the research phase
  verifies whether gating on protocol version is required before an old
  client receives the new variant.
- New client→server dismiss message (dismissal syncs across clients).
- New env override for the GitHub API base URL (test seam, mirrors
  `SCRIBE_UPDATE_API_URL`).
- No CLI surface changes. No breaking changes.

## Testing Strategy

- Unit (server): rollup/aggregation logic, repo-resolution rule
  (push-target remote, fork triangles), ref-state change detection across
  backends (loose, packed-refs, reftable, worktree `.git`-file
  indirection), window open/close/timeout semantics, protocol serde.
- Unit (client): bar segment/state model without a live window, following
  the existing status-bar model test precedent.
- Unit/E2E degrade checks: enabled-but-unauthenticated and disabled
  paths produce zero traffic and a logged reason naming the cause.
- E2E func (`--network none`): local bare repo fixture + scripted push +
  fake GitHub API on 127.0.0.1 driving the full flow — bar appears,
  updates, completes, degrades to stale on API failure, generates zero
  requests when idle or disabled; includes a shared-viewer case proving
  read-only rendering.
- E2E visual: screenshots of collapsed/running/success/failure/stale and
  expanded states against the mockup; frame-stability spot check that an
  active bar does not perturb terminal rendering.
- Manual verification protocol (constitution #3): a documented
  maintainer-run check against a real github.com repo, since the harness
  can never reach the network.
- Research/mockup stories are verified by user approval (their ACs).

## Risks

- ETag/304 rate-limit semantics may have changed — verify against live
  docs in research; fallback is a conservative in-window poll interval.
- Ref-backend matrix (reftable lands in git ≥ 2.45) is the likeliest
  effort doubler — research includes a small ref-watch prototype and
  checks existing crate deps before adding any watcher/git dependency
  (constitution #1).
- Protocol compat: an old client receiving the new message variant must
  not degrade its connection — verify frame-discard behavior and follow
  N/N-1 conventions; mitigation is capability/version gating.
- `gh webhook forward` support status unknown — opt-in only, so a dead
  end there cannot block v1.
- Hot-upgrade continuity — state is re-derivable by design; low risk.
- Rollback: the feature is a leaf behind a default-off toggle; disabling
  it restores current behavior exactly.

## Sequencing

Phase A — research (can start immediately, parallel):
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

Phase B — mockup and approval:
- Mockup (Story 3): static themed HTML/SVG under
  `specs/023-gh-ci-run-bar/mockup/`, all states including
  stale/degraded, non-color signifiers, collapsed + expanded views.
  Depends on UX research. Blocks: approval gate.
- Approval gate: user approves research recommendation + mockup
  direction. Blocks every implementation task.

Phase C — implementation (all blocked by the approval gate; details may
be amended by approved research):
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

## Backlog Refinement

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

## Spec Review

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
