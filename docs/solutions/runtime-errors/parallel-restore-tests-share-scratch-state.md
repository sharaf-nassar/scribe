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

Before `713af6f`, the shared test helper named roots with the process id and
`unix_time_ms()`. Parallel test threads could call it in the same millisecond,
receive the same directory, and exchange one restore index. That explains the
swapped ids, extra entries, and reads of a concurrently truncated TOML file in
`claim_first_window_skips_non_replayable_and_reports_remaining`
(`crates/scribe-client/src/restore_state.rs:708`) and
`stale_claim_becomes_claimable_again`
(`crates/scribe-client/src/restore_state.rs:758`).

## What didn't work

Treating each failure as caused by the staged feature wasted time. The failing
assertions moved between unrelated integrations, and no staged diff touched
`restore_state.rs`. Re-running only the full suite without first checking the
exact tests also obscured the stable signature.

## Fix

`scribe-1hp7` landed as `713af6f`. The fixture now combines the process id with
a process-local atomic sequence (`crates/scribe-client/src/restore_state.rs:614,
829-837`), so live threads cannot choose the same root. A later ponytail review
filed `scribe-u7kp`; `aa97384` removed the unnecessary retry loop while keeping
stale reused-PID cleanup followed by exclusive creation. Focused threaded
stress, full workspace tests, pre-commit, and `lat check` passed for both beads.

## Prevention

On this signature:

1. run each named test with `--exact --nocapture`;
2. compare ids and index contents in the failure to sibling tests;
3. inspect scratch names for clock-only uniqueness before changing product code;
4. stress the whole sibling test group with ordinary parallel test threads;
5. require a final full-suite pass before committing.

Time is not an identity. Test scratch roots shared by parallel callers need a
process id plus an atomic sequence or an operating-system-created unique path.
If stale roots can survive PID reuse, remove the exact stale root before an
exclusive create rather than accepting its old contents.
