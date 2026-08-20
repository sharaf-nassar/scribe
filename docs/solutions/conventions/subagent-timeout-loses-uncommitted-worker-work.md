---
title: The default subagent timeout discards a whole worker's uncommitted work
date: 2026-08-19
component: implement-ready orchestration, tests/e2e visual recipes
tags: [subagents, orchestration, timeout, worktree, e2e, docker, commit-discipline]
problem_type: convention
---

## Problem

In implement-ready run `run-20260818T211904.Pion6e`, a worker on `scribe-re54`
was killed mid-edit:

```text
Subagent timed out after 1800000ms.
```

It had produced a complete implementation — 4 files, ~232 insertions, including
both new visual regression phases — and had committed none of it. A sibling on
`scribe-1x9s` ended the same wave with 7 files and ~561 insertions, likewise
uncommitted, after detaching on a supervisor round-trip.

Nothing was lost only because the rail's task worktrees persist: the changes
were still there as dirty files and a second attempt could resume them. Had the
worktrees been cleaned on failure, an hour of two workers' output would have
been gone.

## Root cause

Two independent causes with the same signature.

`pi-subagents` applies a 30-minute default timeout when the spawn does not set
`timeoutMs`. That is far below what this repo's visual acceptance criteria
need *when the bead changes a binary*: `just docker-visual` builds release
binaries and bakes a ~4 GB image, and only then can `just e2e-visual-<name>`
run. A worker whose bead requires a visual e2e over changed binaries cannot
finish inside 30 minutes on a cold worktree.

That qualifier matters: the recipes bind-mount `tests/e2e`, so a bead that
changes only shell under `tests/e2e` runs its real recipes against the existing
images with no rebuild at all. See
`../environment/e2e-recipes-mount-tests-so-shell-only-changes-skip-the-image-build.md`.
Split the two cases before choosing a budget.

Separately, a worker that blocks on `contact_supervisor` can be detached by the
harness at the round-trip and never resumed, ending the run with the same
"implemented but uncommitted" state. The supervisor reply is delivered, but the
child does not come back.

Both are harness-shaped failures, not task defects, so they leave no useful
`error_signature` and the retry gate cannot distinguish them from real ones.

## Fix

Set `timeoutMs` explicitly on every dispatch that can touch Docker or a release
build. Three hours (`10800000`) cleared it here with room to spare.

Instruct workers to commit incrementally rather than at the end. The wording
that worked:

> COMMIT the Rust fix, the test phases and the lat.md edits as soon as clippy
> and cargo test are green — BEFORE you start `just docker-visual`. Then run the
> visual e2e and commit fixes on top. If the visual e2e cannot finish, still
> return a real commit and report it as not-run in `checks` rather than losing
> the work.

Prefer a returned `status: "failed"` naming the needed decision over a blocking
`contact_supervisor` call. A question that comes back in the result JSON costs
one attempt; a detached child costs the whole attempt's output.

## Recurrence: run `run-20260819T050710.ezoM1n`

It happened again, to two workers in one wave, because the orchestrator
dispatched without applying this file's lesson. `scribe-03wp` and
`scribe-xevt` both returned:

```text
Subagent timed out after 1800000ms.
```

Both had real work uncommitted in their preserved worktrees (30 and 23
insertions), both resumed successfully on attempt 2 with
`timeoutMs: 14400000`, and both then passed. Cost: one wasted wave.

The original trigger — "can touch Docker or a release build" — was too narrow.
The sharper rule is about the acceptance criteria:

> When acceptance demands repeated runs ("passes repeatedly", "ten consecutive
> runs", N-1/N/N+1), the dispatch budget is repetition count times suite
> runtime, never one run.

`scribe-xevt` required ten consecutive `just e2e-func-beads-board` runs at up
to 6m19s each: over sixty minutes of pure suite time against a thirty-minute
budget, so the attempt was arithmetically impossible before it started.
`scribe-03wp` needed an image build plus five shared-pane visual runs.

Read acceptance criteria for a repetition count before choosing `timeoutMs`,
and multiply.

## Prevention

A worker's commit is the only durable artifact of its run. Treat "has it
committed yet?" as the orchestrator's liveness signal — `git -C <worktree>
rev-list --count <base>..HEAD` is cheaper and more informative than polling run
status, and it distinguishes a worker that is building from one that is stuck.

Never clean a task worktree on failure before inspecting it. The rail preserves
them deliberately; a timed-out worker's dirty tree is usually a near-complete
implementation, and resuming it is far cheaper than re-running a container
build from scratch.
