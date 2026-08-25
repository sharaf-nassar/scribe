---
title: Rewriting main mid-run can strand a verified task on an adjacent edit
date: 2026-08-25
component: implement-ready orchestration
tags: [orchestration, rail, rebase, conflict, worktree, git-history]
problem_type: convention
---

## Problem

Implement-ready run `run-20260825T001906.hZDXEb` started task
`scribe-i6pb` from main commit
`5c7b079ac3ae8bc9977dbb47857c4daeb94d738b`. The worker completed every gate
and committed
`f5b7c954716886171d17dac1c1f9553f36e6dd05`, but `prepare` could not replay
that verified commit onto the rewritten main branch:

```text
CONFLICT (content): Merge conflict in specs/020-terminal-images/plan.md
Could not apply f5b7c95... refactor: consolidate terminal image renderer probe
implement-ready: rebase --onto failed; conflict evidence and lock retained
```

The task stayed open and its worktree was preserved.

## Root cause

Main was rewritten after the worker branched. The rail correctly detected that
the recorded base was no longer an ancestor and used `rebase --onto`
(`/home/mamba/.beads/rail/implement-ready.sh:677-679`).

The rewritten main changed the decoder-spike row at
`specs/020-terminal-images/plan.md:208`. The task changed the immediately
adjacent GPUI-spike row at `specs/020-terminal-images/plan.md:209`. Git grouped
both table-row edits into one hunk even though they described different probes,
so replay stopped before the squash could be prepared.

A normal retry is unavailable after this shape of failure. The worker result is
successful, while the retry gate rejects a successor when the previous attempt
did not fail (`/home/mamba/.beads/rail/implement-ready.sh:844-847`).

## Recovery

Run `unlock --abort` immediately after rail exit 5. This releases the
integration lock, aborts the partial rebase, and keeps the original task branch
and conflict evidence. Record the exact conflicting path and error on the bead,
then continue the rest of the frontier.

Do not cherry-pick or resolve directly on main. That bypasses the rail's
verified worker tree, squash preparation, integration gate, cleanup, and audit
record.

The original run cannot turn that successful worker result into a retry: the
retry gate requires a failed prior result. When the conflict is later approved
for repair, file a narrow follow-up bead with the exact conflicting files and
acceptance criteria. Give it a fresh rail worktree from current main, replay the
preserved commit there, inspect every conflict, and run the full normal
result-to-cleanup lifecycle.

That path preserved the retired decoder row and sparse release evidence map
while applying the production-renderer lifecycle changes. Fix record: bead
`scribe-i6pb.1`, squash commit
`22811309403f12e907fe90049f0087b146ab8d36`. The original worker commit
`f5b7c954716886171d17dac1c1f9553f36e6dd05` remains the recovery source.

## Prevention

Make the primary checkout clean and settle any pending history rewrite before
`init`. Once workers branch, do not amend, rebase, or replace main commits that
touch their task files. If main must be rewritten, expect every affected stale
worktree to replay; serialize table, registry, and generated-manifest edits even
when workers change different adjacent rows.
