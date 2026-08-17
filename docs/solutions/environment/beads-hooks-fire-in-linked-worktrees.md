---
title: Beads hooks fire in linked worktrees and can corrupt the live DB
date: 2026-08-17
component: beads-hooks
tags: [beads, git-hooks, worktree, core.hooksPath, data-loss, parallel-runs]
problem_type: environment
---

# Beads hooks fire in linked worktrees and can corrupt the live DB

## Problem

During a 5-way parallel implement-ready run on 2026-08-07, the open P1
bead `scribe-hzk` was hard-deleted from the live beads DB mid-run — not
closed, deleted (total issues fell 347→346 while closed rose 341→346).
It was restored from a run-start snapshot via `bd import`. The tracking
bug filed at the time (`scribe-lcj`, P1) has itself since vanished from
the DB — verified absent on 2026-08-17 (`bd show scribe-lcj` finds
nothing; full-text search over all beads, closed included, has no
hooksPath/worktree-hook entry). Refiled as `scribe-0ful`.

## Root cause

`core.hooksPath` is set to the absolute path
`/home/mamba/work/scribe/.beads/hooks`, and the shims there are tracked
files — so they exist and fire in every linked worktree. The shims run
`bd hooks run post-checkout` / `post-merge` / `pre-commit` / `pre-push`
(`.beads/hooks/post-checkout:9`), which are DB sync operations. Inside a
linked worktree, `.beads/` holds only the tracked JSONL snapshot from
the task branch's base commit; bd resolves the shared live DB from any
worktree. A hook-triggered sync from that stale snapshot against the
live DB is the suspected deletion mechanism (causation was made
plausible, not proven: no agent in the 2026-08-07 run issued any bd
delete/prune — verified by grepping every agent transcript).

A second, milder failure mode is confirmed: a pre-commit `bd export`
inside a worker's worktree writes `.beads/` changes onto the task
branch, which the implement-ready rail's `prepare` then refuses ("task
branch modifies .beads"), killing an otherwise good integration.

Note this is not specific to the absolute path: default-location git
hooks are shared with linked worktrees too. The absolute
`core.hooksPath` just makes it obvious.

## What didn't work

- Suspecting agent bd writes: the 2026-08-07 run grepped every agent
  transcript — the only bd writes were the 5 claims + 5 closes on
  dispatched beads. The deletion happened through git operations, not
  through any agent's bd command.
- Treating it as a one-off: the same config existed unchanged in temet,
  quill, and cue, and the hazard sat untracked after `scribe-lcj`
  disappeared.

## Fix

Landed (machine-local tooling, not this repo's tree): the
implement-ready rail (`~/.beads/rail/implement-ready.sh`) now creates
every task worktree hook-free — `git worktree add --no-checkout` (a
plain add fires post-checkout before any override can exist), then
`extensions.worktreeConfig` plus a per-worktree `core.hooksPath`
pointing at the empty `~/.beads/no-hooks/`, then `reset --hard`. The
/spec and /file worktree protocols use the same sequence. Verified live:
the 2026-08-17 run's worker worktree carries the override. Primary
checkout hooks are untouched — squash commits on main still run the
real hooks, including beads sync, which is where it belongs.

Worktrees created outside that tooling — a manual `git worktree add`,
the superpowers-managed `macos-path-rework` worktree — are neutralized
the same way, one command against the existing worktree:

```sh
git -C <repo> config extensions.worktreeConfig true
git -C <worktree> config --worktree core.hooksPath ~/.beads/no-hooks
```

Every linked worktree on this machine (scribe ×3, cue ×1) was swept this
way on 2026-08-17.

Patching the shims themselves was considered and rejected: it is a
per-repo edit inside beads-managed markers (`BEADS INTEGRATION v1.1.0`),
so it needs mirroring into every beads repo and can be silently clobbered
by the next `bd hooks install`. Fixing the worktree-creating tooling
instead is one change that covers every current and future repo. A bead
proposing the shim campaign was filed and then closed as superseded
(`scribe-0ful`). The durable upstream fix is `bd hooks run` refusing DB
sync from a linked worktree — with the thin-shim design, one bd release
would cover every install.

## Prevention

- Never let a hook that syncs shared mutable state run from a linked
  worktree: the worktree's tracked snapshot is stale by construction.
  Guard on `git rev-parse --git-dir` ≠ `git rev-parse --git-common-dir`.
- Any tooling that creates worktrees should create them hook-free by
  default (`--no-checkout` first — the add itself fires post-checkout).
- When a hazard gets a tracking bead, the bead can be lost to the same
  class of incident it tracks. A learning doc in git is the durable
  record; the bead is the work item.
- Fix a cross-repo hazard in the tooling every repo shares, not in each
  repo's copy of the artifact — especially when that copy is managed by
  an installer that will overwrite it. temet, quill, and cue carry the
  identical absolute `.beads/hooks` config and needed no per-repo change.
