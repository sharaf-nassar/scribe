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
need: `just docker-visual` builds release binaries and bakes a ~4 GB image, and
only then can `just e2e-visual-<name>` run. A worker whose bead requires a
visual e2e cannot finish inside 30 minutes on a cold worktree.

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

## Prevention

A worker's commit is the only durable artifact of its run. Treat "has it
committed yet?" as the orchestrator's liveness signal — `git -C <worktree>
rev-list --count <base>..HEAD` is cheaper and more informative than polling run
status, and it distinguishes a worker that is building from one that is stuck.

Never clean a task worktree on failure before inspecting it. The rail preserves
them deliberately; a timed-out worker's dirty tree is usually a near-complete
implementation, and resuming it is far cheaper than re-running a container
build from scratch.
