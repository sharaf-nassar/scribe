---
title: Duplicate Pi extensions block workers before task tools run
date: 2026-08-17
component: pi-extensions
tags: [pi, extensions, lat, worktree, subagents, startup]
problem_type: environment
---

## Problem

Every worker in implement-ready run `run-20260817T072654.Aira7W`
failed at startup with:

```text
Tool "lat_search" conflicts between user and project .pi/extensions/lat.ts
```

No task tool ran, so retries could not repair either bead. The exact
failure is preserved in the notes for `scribe-555f` and `scribe-onco`.

## Root cause

The repository carried `.pi/extensions/lat.ts` while the same Lat tools
were installed at user scope. Pi rejects duplicate tool names instead of
choosing one extension. Each task worktree was created from a commit that
still contained the project extension, so changing only the primary
checkout did not repair those existing worktrees.

## Fix

Delete the redundant project extension and start a fresh rail run from a
commit that no longer contains it. Commit `ad8ee015` removed
`.pi/extensions/lat.ts`; workers created from that base started normally
and completed both previously stuck beads.

Do not retry workers in stale worktrees after this class of startup
failure. Their committed base still contains the conflicting extension.

## Prevention

- Install a tool extension at either user or project scope, never both.
- Treat pre-tool startup conflicts as environment failures, not task
  failures.
- After removing a project extension, recreate task worktrees from the
  fixed commit; checkout changes do not rewrite existing worktrees.
