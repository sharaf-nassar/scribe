---
title: An early e2e phase failure masks later phases and staled the bug report
date: 2026-08-27
component: tests/e2e visual suites, bug triage
tags: [e2e, triage, phases, stale-report, baseline, beads]
problem_type: convention
---

## Problem

Bead `scribe-b0r9` recorded that `tests/e2e/visual/workspace-drag-tearout.sh`
fails at its **final** phase: `dragging empty chrome did not move the window`.
A worker on `scribe-whn1.2` (run `run-20260827T221244.lFRcOi`) relocated that
final phase, then reported a *different* failure at **phase 4**:

```text
FAIL: top-left-horizontal-tie overlay changed 38136/114744 expected pixels
```

The orchestrator confidently diagnosed this as the worker's own regression —
the relocated phase moves the window, and every helper in that script is
anchored to a once-captured `WIN_X`/`WIN_Y` — and sent it back to fix a
stale-origin bug. That diagnosis was wrong, and cost a full ~10-minute suite
cycle to disprove.

## Root cause

`fail()` ends with `exit 1` (`tests/e2e/visual/workspace-drag-tearout.sh:27`).
A phase-4 failure therefore terminates the run, and **no phase after 4 executes
at all**. Phase 4 broke at some point after `scribe-b0r9` was observed at main
`e987cab`. From that moment the bead's headline — "final phase fails" — became
unreachable and therefore unverifiable, while still reading as current.

So the bead described a real failure that the suite could no longer even reach.
Anyone triaging from the bead text alone inherits a stale premise.

The tell that settled it: the pixel count `38136/114744` was **byte-identical**
before and after the worker's window-restore change. A position-dependent fault
would have moved that number. Identical counts across a state change mean the
fault is independent of that state.

Root cause of phase 4 itself is tracked separately as `scribe-22bu`: the tie
point is built from hard-coded `TITLEBAR_H=34` / `BOTTOM_CHROME_H=24`
(`tests/e2e/visual/workspace-drag-tearout.sh:12`) rather than measured region
geometry, so it is no longer the exact corner tie it computes itself to be.
`zone_at` (`crates/scribe-client/src/workspace_drag.rs:291`) is *not* at fault —
it breaks exact ties toward horizontal, which is what the assertion wants.

## Fix

Run the unmodified script from the primary checkout and compare. That single
command distinguishes "the worker broke it" from "it was already broken", and
it is far cheaper than reasoning about the diff:

```bash
git -C <repo> status --porcelain   # confirm clean
just e2e-visual-<name> 2>&1 | grep -E 'PHASE [0-9]+ PASS|FAIL:'
```

Here that produced `PHASE 0..3 PASS` then the identical phase-4 failure on
untouched main, settling it immediately.

## Recurrence: run `run-20260829T001254.2dsVsH`

This file's baseline rule paid for itself, and the masking went four defects
deep.

A worker on `scribe-cwz9` reported that the beads-card-detail visual recipe
failed before reaching the regression phase it had just added, and claimed the
failure was pre-existing. Rather than reason from the diff, the orchestrator ran
the unmodified recipe on untouched main at `d7eed48`:

```text
FAIL: loading fixture board changed only 1653px
```

The worker had seen 1759px and 1735px on its branch. Three different numbers
across two different trees settled two things at once: the claim was true, and
the counter was *nondeterministic*, which a single observation would not have
shown. `scribe-cwz9` landed on that evidence and the gate was filed separately
as `scribe-h6fh`.

Repairing it exposed the deeper shape of this failure mode. Because `fail()`
exits 1 (`tests/e2e/visual/beads-card-detail-fixtures.sh:12-15`), each fix
unmasked the next defect, one run at a time: a stale hover coordinate, then a
rail sample row, then a separator hairline row, then a time-dependent fixture
staleness race (`e2e-fixture-timestamps-expire-mid-recipe.md`). Four distinct
causes, discoverable only in sequence.

Two things made that tractable rather than an open-ended rewrite:

**Authorize by defect class, not by file list.** The first attempt was told to
stop at the third defect and report it. That was right at the time — it prevented
an unbounded harness rewrite — but it also meant the bead could not finish. The
retry lifted the bound *for stale-constant defects specifically* while keeping
it for anything else, and required every corrected constant to be measured from
a screenshot with before/after readings quoted. The worker then correctly
stopped again at the fourth defect, because a staleness race is not a stale
constant, and asked. That is the shape to reuse: bound by class, re-authorize
explicitly when the class boundary is reached.

**A constant a worker cannot execute is a guess.** `scribe-cwz9` recomputed
`rail_y` from 296 to 321 when it moved a field out of the detail header. The
real row was 320. That off-by-one was invisible because the recipe died before
reaching the assertion, so the worker had no way to check its own arithmetic.
When a suite is dead, treat every layout constant edited against it as unverified
— and prefer fixing the suite first, so later edits are checkable.

## Prevention

**Do a baseline run before accepting any "pre-existing failure" claim, and
before rejecting one.** A worker saying "this was already broken" is a
falsifiable claim with a two-command test. Guessing at causation from the diff
is how a correct worker gets sent to fix someone else's bug.

**Treat a phase number in an old bug report as provenance, not as current
state.** In a suite that exits on first failure, any later-phase report is only
valid while every earlier phase still passes. When re-opening such a bead,
re-run first and re-baseline the phase number.

**Quote the failing assertion's own numbers across attempts.** Identical
counters across a deliberate state change are strong evidence the change is
orthogonal to the fault — the cheapest available signal, and it needs no
instrumentation.

Note that relocating a window-*moving* phase earlier in such a script is still
genuinely unsafe on its own terms, because `shot()` crops to
`${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}`
(`tests/e2e/visual/workspace-drag-tearout.sh:51`) and the drag helpers all add
`WIN_X`/`WIN_Y`. The landed fix in `0623ee0` restores and re-asserts the origin
after proving the native move, which is why phases 1-3 still pass downstream of
it. That precaution was correct; it simply was not the phase-4 cause.
