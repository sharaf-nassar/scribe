---
title: A single-repo rail cannot land an external dependency patch
date: 2026-08-25
component: implement-ready orchestration, GPUI dependency ownership
tags: [orchestration, rail, worktree, external-dependency, gpui, publication]
problem_type: convention
---

## Problem

Implement-ready run `run-20260825T025023.XFFgKK` dispatched
`scribe-99uj.3`, whose declared files were under `crates/gpui/`,
`crates/gpui_linux/`, and `crates/gpui_macos/`. None of those source roots is
tracked by Scribe. Scribe consumes `gpui`, `gpui_platform`, and `gpui_tokio`
directly from the Zed Git repository at one pinned revision
(`Cargo.toml:113-123`).

The bead also required backend tests, an upstream pull request, and an exact
maintained-fork revision. The assigned worktree could therefore produce neither
a Scribe task-branch commit containing the patch nor the publication evidence
required by acceptance.

## Root cause

The rail's ownership and audit model is deliberately single-repository. It
creates a task branch from Scribe, verifies that branch's commit, squashes its
tree onto Scribe main, and records the resulting Scribe commit. A nested or
sibling Zed clone would have a different Git history, so its commit cannot pass
that lifecycle or appear in the prepared Scribe tree.

Publication authority is a separate boundary. Constitution principle 7 forbids
forking, pushing, or opening a pull request without explicit authority. Even
with that authority, the current Scribe rail still could not integrate or audit
the external commit.

## What didn't work

Treating an acceptance-complete ready bead as mechanically dispatchable consumed
one worker attempt before the repository mismatch was surfaced. Authorizing the
worker to publish would not have fixed the rail mismatch; it would only have
added an unaudited second repository to a Scribe-scoped task.

## Fix

The user marked `scribe-99uj.3` stuck with the stable signature:

```text
ownership check: crates/gpui source roots absent from assigned Scribe worktree
```

Blocker `scribe-fit7` now requires an owned Zed/GPUI checkout, a repository-local
work rail, an explicit maintained-fork target, and publication authority. The
external patch must land and pass backend tests there first. A later Scribe task
can then repin the exact revision and integrate through Scribe's rail.

No squash commit exists for the stuck task because no valid Scribe-tree change
was possible. Related Scribe-side negotiated-mode work landed separately as
`22739ed` for bead `scribe-99uj.1`; it does not resolve the external ownership
gap.

## Prevention

Before claiming a task, compare every declared source root with the repository's
tracked paths and dependency declarations. If the implementation files live in
a Git dependency rather than the scoped repository, split the work before
dispatch:

1. External repository task: patch, test, publish, and record the exact revision.
2. Consumer repository task: repin, adapt, and run integration gates.

Do not use a nested clone to make a single-repository rail appear to own both
halves. The missing squash and audit trail are the signal that the task boundary
is wrong.
