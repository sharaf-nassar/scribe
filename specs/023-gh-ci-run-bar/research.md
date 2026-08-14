# gh-ci-run-bar research

Research artifact for `specs/023-gh-ci-run-bar.md`. Covers the ownership
conclusion (Story 1 milestone) and the UX design rules (Story 2). The
transport/trigger verification (Story 1 remainder, bead scribe-gygu.2) is
still pending and will be appended here — its ETag/webhook/ref-backend
claims must be verified against live GitHub docs before the approval gate.

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
  active tracked run: keep the last-known content, add `⚠ stale · retry
  in Ns`, stop advancing elapsed. Recovery resumes running; window
  deadline expiry goes to hidden. Outside an active run, failures never
  surface a bar.
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
status row. Collapsed, a segmented pipeline strip carries one named
segment per job (micro-label above each segment; workflow boundaries as a
wider gap), with a conic ring filling as jobs complete. The active job is
marked three ways at once: its segment shimmers, its name brightens to
full foreground, and a breathing ▸ precedes it; done jobs dim to solid,
queued jobs dash, a failed job's name goes red. Beyond ~6 jobs the strip
elides to failed + active jobs plus a "+n" counter segment. Expanded,
jobs render as time-positioned bars on a shared minute-grid axis so
parallelism reads at a glance. A hairline under the band takes the
workspace badge color — ownership encoded structurally, which is what
disambiguates the band in multi-workspace windows. Failure/stale re-key
that hairline semantically; stale freezes all motion while keeping the
active job named. Motion (appear sweep, shimmer, breathing caret, success
exit) routes through the existing GPUI animation system and its off
switch; terminal states are static. An alternate **edge** direction
trades the band for a 2px luminous progress line under the titlebar plus
a fading micro cluster naming the active job — zero chrome height; both
directions share the trace panel.

### Notification surface decision

The bar's appearance is the v1 notification. The client's existing
desktop-notification wiring (bell path) is the natural post-v1 opt-in
for run completion, but v1 adds no desktop notifications (spec
Non-Goal); no other in-app surface is repurposed.

## Transport & trigger verification (Story 1 remainder)

Pending — tracked as bead scribe-gygu.2. Must verify against live GitHub
docs: ETag/304 rate-limit accounting, `gh webhook forward` support status
and permissions, fine-grained PAT scopes for Actions reads, plus the
ref-watch prototype across loose refs / packed-refs / reftable /
worktree indirection, protocol N/N-1 gating needs, and final numeric
bounds. The approval gate (scribe-gygu.5) stays blocked until this
section is filled in.
