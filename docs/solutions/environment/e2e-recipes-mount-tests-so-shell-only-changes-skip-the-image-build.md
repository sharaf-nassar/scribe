---
title: E2E recipes bind-mount tests/e2e, so shell-only changes need no image rebuild
date: 2026-08-20
component: tests/e2e, justfile, implement-ready orchestration
tags: [e2e, docker, justfile, worker-budget, shell, verification]
problem_type: environment
---

## Problem

A bead that touches only `tests/e2e/**` shell recipes looks unaffordable to
verify. The standing guidance in
`docs/solutions/conventions/subagent-timeout-loses-uncommitted-worker-work.md`
is that visual acceptance needs `just docker-visual` — a release build plus a
~4 GB image bake — before `just e2e-visual-<name>` can run, and that a worker
cannot finish that inside a default timeout.

Applied bluntly, that pushes an orchestrator into accepting `bash -n` as the
only check for shell refactors, which cannot show that a recipe still passes.

## Root cause

The rebuild is only required when the **binaries** change. The scripts are not
baked into the image — they are mounted at run time. Both recipes in
`justfile` end with the same shape:

```
docker run --rm ... -v ./tests/e2e:/tests:ro ... "$image" /tests/{{ script }}
```

`e2e-func` at `justfile:210` and `e2e-visual` at `justfile:287` each mount the
working tree's `tests/e2e` over `/tests` inside the container. Neither recipe
depends on a build target, so neither triggers one.

So for a change that touches only shell under `tests/e2e`, the prebuilt
`scribe-test-func` and `scribe-test-visual` images already on the host are
valid, and the edited scripts are picked up directly from the worktree.

## Fix

Check for the images first:

```bash
docker images | grep scribe-test
```

If `scribe-test-func` / `scribe-test-visual` exist, run the affected recipes
straight from the worktree, with no build step:

```bash
just e2e-func func/agent-read.sh
just e2e-visual agent-indicator.sh
```

The mount is `./tests/e2e`, relative to the shell's CWD, so this must be run
from the worktree root for the worktree's edits to be the ones under test.
Running it from the primary checkout silently tests main's scripts instead.

In `run-20260820T054159.ub1CzZ` this let `scribe-9epu` baseline and then re-run
all six of its agent recipes — three functional, three visual — inside one
ordinary worker attempt, and it caught that `agent-consent-dialog.sh` was
already flaky at baseline, which `bash -n` could never have shown.

## Prevention

When scoping a `tests/e2e` bead, split the question in two: does the change
alter a **binary**, or only a **script**? Only the first needs
`just docker-func` / `just docker-visual`.

Tell the worker explicitly which case it is and forbid the other, since a
worker that reaches for `just docker-visual` on a shell-only task will burn its
whole budget on an image it did not need. Pair it with a baseline run of each
recipe before the edit, so pre-existing flakiness is attributed correctly
instead of being chased as a regression.
