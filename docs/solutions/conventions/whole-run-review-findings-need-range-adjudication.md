---
title: The whole-run review agent cannot run git diff, so every finding's
  in-range status is the orchestrator's to prove
date: 2026-08-29
component: implement-ready orchestration (ponytail-review pass)
tags: [orchestration, implement-ready, ponytail-review, subagents, parallel-runs, adjudication]
problem_type: convention
---

## Problem

`implement-ready` step 6 dispatches one agent to run `ponytail-review` over
`git diff RUN_BASE..HEAD`. In run `run-20260829T070023.yeTJNC` that agent was
spawned as the builtin `reviewer`, which has **no shell tool**. It could not run
the command it was asked to review. Its own acceptance report says so:

> `git diff b8d8aa7..c01e728` — `"result": "not-run"` — "No shell tool in this
> review role. Reconstructed the post-state by reading files at HEAD after
> confirming `.git/HEAD` -> `refs/heads/main` -> `c01e728…` and `.git/logs/HEAD`
> showing `b8d8aa7 -> 8aa7cd3 -> d6c6beb -> c01e728`."

It recovered well — reading `.git/logs/HEAD` to establish the range is genuinely
clever — but a reconstruction from the HEAD *tree* cannot distinguish code the
run introduced from code that was already there. The agent knew this and said
so, flagging as a residual risk that "a supervisor should confirm finding 4's
function body and finding 3's two test bodies are inside the range rather than
pre-existing."

This matters in both directions, and both directions actually fired:

- Two items the reviewer **demoted to notes** because it could not confirm they
  were in range turned out to be in range and run-introduced. It nearly dropped
  its own best findings.
- Findings it stated confidently still needed range confirmation before any of
  them justified a bead.

## Root cause

A read-only reviewer role is the right call for a review pass — it cannot edit,
commit, or drift. But "read-only" in this harness also means no `bash`, and
`git diff A..B` is the entire input to the task. The orchestrator prompt names
the range; nothing guarantees the spawned role can execute it.

## Fix

Treat the review agent's output as **located inferences, not range-verified
facts**. Adjudicate each one parent-side with two cheap commands:

```bash
# Did this run introduce the line at all?
git diff RUN_BASE..HEAD -- <file> | grep -E '^[-+].*<symbol>'

# What did the pre-run state actually look like?
git show RUN_BASE:<file> | sed -n '<start>,<end>p'
```

That second command settled the run's most consequential question. Finding 1
proposed deleting `workspace_tree_has_leaf`
(`crates/scribe-server/src/workspace_manager.rs:798`) as a duplicate of the
private `workspace_tree_contains`
(`crates/scribe-common/src/protocol.rs:1992`) — which appeared to contradict
`scribe-kzah`'s acceptance criteria, which forbade widening "the non-equivalent
private common helper at `protocol.rs:1983-1993`". `git show b8d8aa7:` on that
exact range showed those lines were `workspace_tree_leaf` — a *different*,
first-match, `Option`-returning helper (now at `protocol.rs:2011`). The bead's
prohibition was correct and aimed elsewhere; the finding was not barred by it.
Neither the reviewer nor the bead text alone could establish that.

Two notes the reviewer could not place in range were confirmed run-introduced
and fixed in `f81b6b1` (`scribe-f2q9`):

- `crates/scribe-client/src/main.rs:6843` documented "splits and closes
  re-equalize on their own", which `d6c6beb` (`scribe-5dke`) made false by
  removing the implicit equalize calls.
- `WorkspaceManager::active_index_or` had two callers at `b8d8aa7`
  (`workspace_manager.rs:702` with a computed fallback, `:751` with `0`).
  `8aa7cd3` (`scribe-5v79`) routed the first through the new shared departure
  helper, leaving one caller that always passes `0` — a parameter for a value
  that never varies, created by the run itself.

## Why the pass still pays: cross-task convention drift

The review's real find was only visible in the combined diff. In one run,
`scribe-5v79` established "shared pure logic goes `pub` in
`scribe-common/protocol.rs` and both sides call it"
(`protocol.rs:1898` consumed by `tab_session.rs:136` and
`workspace_manager.rs:701`), while `scribe-kzah` — in the same run, editing the
same server file — hand-rolled a private server-local copy of a traversal that
already existed in `protocol.rs`.

Neither worker was wrong on its own branch; neither could see the other's
unlanded work. No per-task review would ever surface it, because no single task
diff contains both halves. That is the whole justification for the one-agent
whole-run pass, and it is worth keeping even though its findings need
adjudication. Filed as `scribe-9d7d`.

## Prevention

- Spawn the review agent with a role that has a shell, or accept that in-range
  claims are unverified and budget the orchestrator greps.
- Require the reviewer to report, per finding, the evidence it actually
  executed versus inferred. This one did, unprompted, and that honesty is what
  made its notes recoverable instead of noise.
- Read the reviewer's demoted "notes" and "could not confirm" section as
  carefully as its numbered findings. In this run the notes contained two real
  run-introduced defects and the numbered findings contained one design
  reversal that had to be deferred (`scribe-9d7d`) and one item refuted
  outright: inlining `sever_moved_session_routes`
  (`crates/scribe-server/src/ipc_server.rs:8432`) was rejected because
  `scribe-kzah`'s acceptance criteria explicitly require that function to exist
  and delegate, and its doc comment carries a non-obvious invariant about the
  unconditional resize-pacer discard.
