---
title: Prove an E2E fail-before by running the new script against the old image
date: 2026-08-22
component: tests/e2e/visual, justfile (e2e-image-current), implement-ready orchestration
tags: [e2e, docker, justfile, verification, regression-test, acceptance]
problem_type: environment
---

## Problem

A bug bead's acceptance criteria routinely say "a regression test that fails
before the fix and passes after". The pass-after half is easy. The fail-before
half looks expensive: it seems to require checking out the pre-fix tree,
rebuilding the binaries, baking a ~4 GB image, running the suite, then undoing
all of it — so it quietly gets skipped, and a test that never demonstrated the
defect gets accepted as its regression test.

A test nobody watched fail is not evidence. It can assert something the bug
never violated and still pass after the fix.

## Root cause

The two halves of a visual E2E run come from different places, and only one of
them needs a rebuild:

- **Scripts are mounted at run time.** Every recipe ends with
  `-v ./tests/e2e:/tests:ro` (for example `justfile:443`), so the container
  runs the working tree's script, not a baked copy. This is the mechanism
  `e2e-recipes-mount-tests-so-shell-only-changes-skip-the-image-build.md`
  already documents.
- **Binaries are baked into the image.** They change only when
  `just docker-visual` restages `target/e2e-stage/<profile>` and rebuilds.

So a *stale image plus a fresh script* is not merely a hazard to avoid — it is
exactly the fail-before configuration, available for free. The pre-fix client
is already sitting on the host in `scribe-test-visual:latest`.

The one obstacle is the currency guard. `e2e-image-current` (`justfile:200-211`)
compares the image's `scribe.e2e.inputs` label against the working tree's hash
and hard-fails:

```
ERROR: E2E image scribe-test-visual does not match this working tree.
```

Every `e2e-visual-*` recipe now depends on it (`justfile:350-592`), so
`just e2e-visual-<name>` will refuse to run the before case. That guard is
correct — it exists to stop false greens — but it must be bypassed deliberately
for this one purpose.

## Fix

Run the before case with the recipe's own `docker run` line, taken from the
justfile body rather than through `just`:

```bash
# BEFORE: pre-fix image already on the host + the new script, mounted
docker run --rm --network none -e TEST_TIMEOUT=240 \
    -v ./tests/e2e:/tests:ro \
    -e HOST_UID=$(id -u) -e HOST_GID=$(id -g) -v ./test-output:/output \
    scribe-test-visual /tests/visual/cold-restart.sh

# AFTER: restage the fixed binaries, then go through just so the guard passes
just docker-visual
just e2e-visual-cold-restart
```

Expand `{{ gpu_flags }}` (`justfile:143`) and `{{ e2e_output }}`
(`justfile:153`) by hand, as above.

Read `test-output/result.log` for the phase line rather than the container's
tail — the failing assertion is one line among thousands of tracing frames.

In `run-20260822T201715.qp3WCy` this proved `scribe-ltx9` (fix in `f5ca195`).
The extended three-pane, two-region `tests/e2e/visual/cold-restart.sh` run
against the 15-hour-old pre-fix image gave phases 0-4 green and then:

```
FAIL: PHASE 5: replay moved a session into another workspace region
```

After `just docker-visual`, the same script passed every phase. That is the
whole fail-before/pass-after proof, and the only build it cost was the one the
fix needed anyway.

## When it does not apply

The before image must be *pre-fix but otherwise close enough* that earlier
phases still pass. If phases 0-4 fail on the old image, the signal is muddy and
the run proves nothing — rebuild a real baseline from the pre-fix commit
instead. Check the age first:

```bash
docker images --format '{{.Repository}}:{{.Tag}} {{.CreatedSince}}' | grep scribe-test
```

This is also worth splitting across seats. A task worktree has a cold cargo
target directory, so restaging a release build there is punishing; the
orchestrator's primary checkout has the warm target and the existing image.
Telling the worker to write the E2E script but not run it, and running both
halves of the proof in the primary checkout afterwards, costs one release
rebuild instead of two full ones.

## Prevention

When a bead's acceptance says "fails before, passes after", name in the bead
*which* half runs where. Report the before-run's exact failing line in the
close reason, so the next reader can tell a demonstrated regression test from
an asserted one.
