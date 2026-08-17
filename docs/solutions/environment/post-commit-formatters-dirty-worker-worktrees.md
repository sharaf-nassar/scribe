---
title: Post-commit formatters can leave worker worktrees non-canonical
date: 2026-08-17
component: implement-ready
tags: [subagents, formatter, worktree, verification, retry]
problem_type: environment
---

# Post-commit formatters can leave worker worktrees non-canonical

## Problem

In run `run-20260817T080435.ASYWT5`, workers for `scribe-555f` and
`scribe-onco` committed tested fixes, then finished with formatter-only
tracked changes in their worktrees. `verify-worker` could not accept the
branches because their worktree state no longer matched the reported
commits.

The evidence is preserved in:

- `attempts/scribe-555f/1/result.json`
- `attempts/scribe-onco/1/result.json`

under the run directory in `~/.local/state/bd-orchestrate/`.

## Root cause

A formatter pass occurred after each worker's commit. The commit remained
the tested canonical change, but the later formatting dirt made the
checkout ambiguous: integrating the commit would omit visible worktree
changes, while integrating the worktree would bypass the reported SHA.

## Fix

Inspect the residual diff before retrying. For both tasks it contained
only post-run formatting noise, so the recovery attempts restored the
tracked files to `HEAD`, preserved the tested commits, and proved clean
status before rail verification. The attempt-2 records identify the
unchanged canonical SHAs.

If residual changes affect behavior, do not restore them. Commit the
formatted result, rerun affected tests, and report the new SHA.

## Prevention

- Run formatters before the final commit.
- Make `git status --short` the last worker check after all tool calls.
- Require a clean worktree and a full canonical SHA before recording a
  successful rail result.
- Classify residual diffs before recovery; only proven formatter noise is
  safe to discard.
