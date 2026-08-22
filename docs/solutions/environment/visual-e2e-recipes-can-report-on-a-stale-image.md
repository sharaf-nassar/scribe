---
title: A visual E2E recipe with no image dependency reports on a stale binary
date: 2026-08-21
component: justfile (e2e-visual-* recipes), tests/e2e/visual
tags: [e2e, docker, justfile, stale-image, false-green, attribution]
problem_type: environment
---

## Update (2026-08-22): the gap described here is now guarded

`e2e-image-current` (`justfile:200-211`) compares the image's
`scribe.e2e.inputs` label against the working tree's hash and hard-fails when
they differ, and every `e2e-visual-*` recipe now depends on it — including
`e2e-visual-beads-board` (`justfile:592`). A casual run can no longer answer
about an older image; it refuses to run at all. The counter-pressure below was
resolved the way it asks for: the guard compares inputs rather than forcing a
rebuild, so bind-mounted `tests/e2e` edits still need no image bake.

The reasoning below is kept because the failure mode it describes is what the
guard exists to prevent, and because deliberately running a fresh script
against a stale image is now a *technique* rather than an accident — see
`fail-before-proof-runs-the-new-script-on-the-old-image.md` in this directory.

## Problem

`just e2e-visual-beads-board` had no `docker-visual` dependency. It runs against
whatever `scribe-test-visual` image already exists, which may have been built
from an unrelated tree. The suite then reports a confident pass or fail about a
binary that has nothing to do with your working copy.

This produced a real false green. An orchestrator ran the suite on main, saw it
pass, and wrote "visual passes on main" into a downstream bead as an established
fact. It did not pass. A worker later stashed its own edits, ran
`just docker-visual` explicitly, reran, and got a reproducible failure with an
identical pixel count — which is how a genuine regression
(a cached board view repainting one frame late, so the hover-grace drawer never
closed) was found at all. It had been sitting red on main, invisible, because
every casual run answered about an older image.

## Why it is worse than a missing test

A missing suite is a known gap. A suite that can silently answer about the wrong
binary launders a stale result into evidence: it gets quoted in a bead, a commit
message, or a handoff, and the next person reasons from it. Attribution is the
thing an E2E suite exists to provide, and this recipe cannot provide it.

## What to do

Before trusting any visual E2E result, rebuild:

```bash
just docker-visual && just e2e-visual-beads-board
```

When you report a result, say whether the run was preceded by a rebuild. A green
you cannot attribute to your own binary is not evidence.

To prove attribution for a suspected pre-existing failure, do what caught this
one: stash your changes, `just docker-visual` from the clean tree, rerun, and
compare the failure signature — a byte-identical pixel count is strong evidence
the failure predates your work.

## The counter-pressure

Do not simply add `docker-visual` to every recipe. Rebuilding is slow, and
`e2e-recipes-mount-tests-so-shell-only-changes-skip-the-image-build.md` in this
directory documents that `tests/e2e` is bind-mounted, so shell-only edits
legitimately need no rebuild and that fast path is worth keeping. The fix has to
distinguish "the shell script changed" from "the Rust changed", and only force a
rebuild for the second. Tracked as its own bead.

## Sibling recipes

This was found in one recipe; it is a recipe-shape problem, not a one-off. Audit
any `e2e-visual-*` or `e2e-func-*` recipe before trusting it, and check whether
it can run against an image it did not build.
