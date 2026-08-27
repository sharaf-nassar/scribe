---
title: Workflow-launched subagent children inherit a 30-minute timeout and die
  mid-edit with nothing committed
date: 2026-08-27
component: pi-subagents (implement-ready orchestration)
tags: [pi-subagents, orchestration, timeout, parallel-runs, implement-ready]
problem_type: environment
---

## Problem

`pi-subagents` gives a run a default deadline of 1800000 ms (30 minutes) when no
explicit timeout is supplied. Children launched from inside a `workflowScript`
via `runs.all([...])` are subject to that default individually — the workflow
itself having "no default parent deadline" does not exempt them.

On 2026-08-27 two of the five workers in the first wave of an `implement-ready`
run were killed at exactly 1800000 ms:

```
Subagent timed out after 1800000ms
```

Both were large beads. `scribe-uc1y` had already written 807 insertions across
10 files and `scribe-2ynl` 411 insertions across 5, and **neither had committed
anything**. The work survived only because the rail preserves task worktrees, so
the diffs were sitting uncommitted in the working tree.

The failure is easy to misread. It arrives looking like a worker problem, and
the natural reaction is to escalate the model or re-dispatch fresh. Neither is
right: nothing about the code or the task was wrong, and a fresh re-dispatch
throws away work that is largely sound.

## Fix

Pass an explicit budget on the top-level `subagent` call. It does propagate to
workflow children:

```
subagent({ async: true, timeoutMs: 14400000, workflowScript: "..." })
```

Verify rather than assume — the effective value is recorded per run:

```bash
python3 -c "import json;print(json.load(open(
  '/tmp/pi-subagents-uid-1000/async-subagent-runs/<run-id>/status.json'))['timeoutMs'])"
```

A 4-hour budget read back as `14399769`, confirming propagation. A single
`status` call does not show the deadline, so this file is the check.

Sizing: any bead whose acceptance criteria require a Docker visual regression
needs hours, not minutes. A single `just docker-visual` is a release build of a
GPUI workspace, and a fail-before/pass-after proof needs at least two of them,
serialised behind a shared lock against every sibling doing the same
(`parallel-workers-share-one-docker-image-tag.md`). Thirty minutes does not fit
one image build under contention.

## Recovering a timed-out worker

The work is not lost. Do not restart from scratch:

1. The task worktree is preserved and dirty. Commit it as a checkpoint
   (`git -C <worktree> add -A` then a plain `chore:` commit).
2. Rebase it onto whatever siblings landed while it was running, so the retry
   starts from current `main` instead of a stale branch point.
3. Record the attempt as failed through the rail with a stable
   `error_signature` naming the harness timeout, so the retry gate sees a real
   prior failure rather than a missing result.
4. Re-dispatch with a larger budget and tell the worker plainly that its own
   prior work is sitting in `HEAD`, that the kill was a harness timeout rather
   than a code failure, and that it should read and finish that work rather
   than revert it.

Attempt 2 of `scribe-2ynl` did exactly this and additionally found two real
defects in attempt 1's own uncommitted code, which a from-scratch redo would
have silently reintroduced or silently dropped.

## Also tell workers to commit early

The deeper lesson is not the number. A worker that treats "commit" as the last
step before reporting has a single point of total loss. Instruct workers to
reach a committed, green state before polishing, and to commit what is green and
report partial coverage if they are running long. That converts a hard timeout
from total loss into a partial result.
