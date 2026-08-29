---
title: A fixture timestamp stamped once at build expires mid-recipe and wipes
  the fixture
date: 2026-08-29
component: tests/e2e visual suites, scribe-client Beads board
tags: [e2e, visual, fixtures, staleness, timing, beads-board, flaky]
problem_type: convention
---

## Problem

`tests/e2e/visual/beads-card-detail-fixtures.sh` was dead on main: every phase
after the loading gate was unreachable. The visible symptom looked like a
rendering bug:

```text
FAIL: loading fixture board changed only 1653px
```

Repairing that unmasked a chain of stale layout constants, and behind those, a
failure of a completely different kind: the recipe's *later* phases failed
because the test had been running too long.

## Root cause

The client refreshes a Beads board on hover when its snapshot is older than
`BEADS_HOVER_REFRESH_AGE` (30s, `crates/scribe-client/src/main.rs:933`). The
fixture stamped `refreshed_at_epoch_ms` exactly once, when `DETAIL_MESSAGES`
was built, and every later injection replayed that same original timestamp.

So the fixture aged in real time while the recipe ran. Hover ages measured from
run artifacts within a single run:

| phase                | age at hover |
| -------------------- | ------------ |
| loading              | +3s          |
| Escape dismissal     | +27s         |
| close-mark dismissal | +29s         |
| backdrop dismissal   | +30.5s       |

The backdrop phase was the first hover past the cliff. There the client did
exactly what it is designed to do: it issued a real `RequestBeadsBoard`, got an
instant `NotDetected` reply (the container has no project), and wiped the
fixture board before the phase could click it. `share-wire.jsonl` confirms the
request fires at that hover and at no earlier one.

Nothing was wrong with the client, and nothing was wrong with the failing
phase's logic. The failure was **time-dependent, not logic-dependent**. The
trigger was that commit `6d8d8fa` had added a long-design pixel comparison
earlier in the same recipe, costing roughly 10 extra seconds — enough to push
the first post-30s hover from just under the cliff to just over it.

## Fix

Restamp on every injection instead of once at build
(`tests/e2e/visual/beads-card-detail-fixtures.sh:146-153`). Landed in
`425bea7` for bead `scribe-h6fh`.

The fixture author's intent was already "this snapshot is fresh" — they stamped
`time.time()` at build. The defect was only that the stamp did not follow the
fixture through its reuse.

Do not fix this by widening `BEADS_HOVER_REFRESH_AGE`. Adjusting production
staleness behavior so a test can keep up inverts the relationship between the
product and its harness.

Recorded tradeoff: a fixture that is always fresh no longer exercises the
hover-refresh-on-stale-board path anywhere in this recipe.

## Prevention

**A fixture carrying a timestamp is a fixture with an expiry date.** Any
injected snapshot the product will compare against `now` must be restamped at
injection, not at construction. Before adding phases to a suite, grep its
fixture builders for `epoch`, `_at`, and `time.time()`.

**Wall-clock runtime is part of an e2e recipe's contract.** This suite was one
slow assertion away from breaking for its entire life. Adding a phase to a
script whose fixtures have a staleness cliff can break a *later, unrelated*
phase, and the failure surfaces nowhere near the change that caused it. When a
recipe grows a slow assertion, re-check total runtime against every staleness
threshold the product applies.

**The tell is a phase that fails by position rather than by content.** When
dismissal phases that are structurally identical to earlier passing ones fail
only because they run last, measure elapsed time before you read the phase.
