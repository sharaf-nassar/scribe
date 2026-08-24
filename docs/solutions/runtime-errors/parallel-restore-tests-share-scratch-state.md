---
title: Parallel restore-state tests can exchange scratch index state
date: 2026-08-24
component: scribe-client restore_state tests
tags: [tests, restore-state, tempdir, concurrency, flake]
problem_type: test-flake
---

# Parallel restore-state tests can exchange scratch index state

## Problem

Full workspace and pre-commit test runs intermittently failed two restore tests
while every exact rerun passed:

- `claim_first_window_skips_non_replayable_and_reports_remaining`
  (`crates/scribe-client/src/restore_state.rs:702-704`);
- `stale_claim_becomes_claimable_again`
  (`crates/scribe-client/src/restore_state.rs:752-754`).

Observed failures included swapped `WindowId` values, extra index entries, and
an empty index TOML missing its `version`. Immediate isolated reruns and later
full reruns were green.

## Root cause

The signatures show cross-test scratch-state interference rather than a product
regression: each test creates nominally isolated state, yet the failures contain
the other test's generated ids or a concurrently truncated shared index. The
exact collision mechanism still needs a focused stress reproduction; follow-up
bug `scribe-1hp7` owns that work.

## What didn't work

Treating each failure as caused by the staged feature wasted time. The failing
assertions moved between unrelated integrations, and no staged diff touched
`restore_state.rs`. Re-running only the full suite without first checking the
exact tests also obscured the stable signature.

## Prevention

On this signature:

1. run each named test with `--exact --nocapture`;
2. compare ids and index contents in the failure to sibling tests;
3. file the shared-state defect rather than changing unrelated code;
4. require a final full-suite pass before committing.

The durable fix should give every restore-state test a unique scratch root and
stress the pair under ordinary parallel test threads. Until `scribe-1hp7`
lands, isolated success plus a clean full rerun distinguishes this known flake
from an integration regression.
