---
title: Parallel workers share one Docker image tag, so a fail-before baseline can
  be a sibling's binary
date: 2026-08-27
component: justfile (docker-visual, e2e-visual-* recipes), parallel implement-ready runs
tags: [e2e, docker, parallel-runs, stale-image, false-green, flock, attribution]
problem_type: environment
---

## Problem

`just docker-visual` writes a fixed global tag, `scribe-test-visual:latest`, and
every `e2e-visual-*` recipe runs that one tag (for example `justfile:429`). The
tag is a machine-wide singleton. Nothing in the recipe is scoped per worktree,
per branch, or per run.

That is fine for one developer. It breaks in two distinct ways once more than
one worker is building images on the same host, which is exactly what a parallel
`implement-ready` run does.

### 1. Concurrent siblings overwrite each other's tag

Two workers in separate worktrees each run `just docker-visual`. Both write
`scribe-test-visual:latest`. Whoever finishes second wins, and the first
worker's subsequent suite run photographs the *other* worker's binary. Both
workers see a plausible result and neither sees an error. The suite is green or
red for reasons that have nothing to do with the diff being tested.

### 2. A retry's "before" baseline is the previous attempt's binary

This is the subtler one and it actually happened on 2026-08-27, during
`scribe-2ynl`. Attempt 1 was killed by a harness timeout after it had already
built an image from its own partial work. Attempt 2 resumed, ran a fail-before
baseline expecting to see unmodified `main` behaviour, and instead measured
attempt 1's leftover binary sitting in the shared tag. The "before" number was
therefore not a `main` number at all.

The worker caught it, rebuilt an explicit pre-fix baseline, and only then had a
real 16.0-versus-23 fail-before/pass-after proof. Had it not noticed, the bead
would have shipped with a fabricated baseline and a green suite.

This is a different failure from
`visual-e2e-recipes-can-report-on-a-stale-image.md`. There, the image was stale
because nobody rebuilt it, and `e2e-image-current` (`justfile:200-211`) now
hard-fails on that. Here the image is *fresh* — it carries a correct
`scribe.e2e.inputs` label for the tree that built it — it just belongs to
somebody else's tree. The currency guard compares inputs against the working
tree that invokes it, so a sibling's fresh image can still pass the guard from a
different worktree, and a same-worktree retry certainly does.

## Fix

Serialise image build and suite run together, under one lock, so no one can
swap the tag between your build and your run:

```bash
flock /tmp/scribe-e2e.lock -c 'just docker-visual && just e2e-visual-<recipe>'
```

The `&&` inside the single `flock` invocation is the whole point. Locking only
the build, then running the suite outside the lock, reintroduces the race in a
narrower window.

Two further rules that follow from it:

- **Never inherit a baseline.** For any fail-before/pass-after proof, build the
  pre-fix image yourself inside the lock in the same session that measures it.
  Do not trust whatever is in the tag, even — especially — if a previous attempt
  of *your own bead* put it there.
- **Shell-only changes still need a trustworthy image.** `tests/e2e` is bind
  mounted (`e2e-recipes-mount-tests-so-shell-only-changes-skip-the-image-build.md`),
  so iterating on a script needs no rebuild. But the *binary* in the tag must
  still be the one you mean to test. Build once under the lock at the start,
  then iterate on the mounted script.

## Why not per-worker tags

Tempting, and it would remove the race outright. It was not done because every
`e2e-visual-*` recipe, the `e2e-image-current` guard, and the CI job names all
hard-code the tag, so parameterising it touches far more than the problem
warrants. A `flock` is one line at the call site and costs only wall-clock time,
which a parallel run has to spend anyway since these builds saturate the host.

Revisit if workers start needing genuinely different images at the same time
rather than the same image serially.
