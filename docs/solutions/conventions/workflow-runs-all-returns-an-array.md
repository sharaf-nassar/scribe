---
title: runs.all returns an array, and indexing it by key discards every worker result
date: 2026-08-20
component: implement-ready orchestration, pi-subagents workflowScript
tags: [subagents, orchestration, workflowscript, runs-all, worker-results, recovery]
problem_type: convention
---

## Problem

In implement-ready run `run-20260820T054159.ub1CzZ`, a three-worker wave was
dispatched as one `workflowScript`. All three workers finished, passed their
own gates, and committed. The workflow itself then failed:

```text
TypeError: Cannot read properties of undefined (reading 'output')
    at workflow-script.js:193:37
```

The orchestrator received no worker JSON at all — no `commit_sha`, no `checks`,
no `summary` — for three tasks that had each fully succeeded.

## Root cause

The script ended with a key-indexed lookup:

```js
const out = await runs.all([
  { key: "scribe-9epu", agent: "worker", task: t9epu },
  { key: "scribe-yp32", agent: "worker", task: typ32 },
  { key: "scribe-mh3o", agent: "worker", task: tmh3o },
]);
return { "scribe-9epu": out["scribe-9epu"].output };   // undefined.output
```

`runs.all` resolves to an **array** of result objects, not a map keyed by the
`key` field. Each element carries its own `key`, so the key is available for
correlation, but only by reading it off the element. The skill spells this out
at `references/execution-controls.md:74`:

```js
return reviews.map(result => result.output);
```

`out["scribe-9epu"]` is therefore `undefined`, and `.output` on it throws. The
throw happens in the return expression — after every child has completed — so
the failure is maximally misleading: the run reports `state: failed` while
`completion-replay` records all three children as `success: true`.

## Fix

Correlate by reading `key` off each element:

```js
const out = await runs.all([...]);
return Object.fromEntries(out.map(r => [r.key, r.output]));
```

## Recovery when it has already happened

The results are not actually lost, because the crash is in the parent's return
expression rather than in the children:

1. Worker commits survive in the rail's task worktrees. `git -C <worktree> log`
   gives the authoritative `commit_sha` — better evidence than the worker's own
   claim, which is what `verify-worker` exists to check anyway.
2. Each child's full final message, including the result JSON it tried to
   return, is on disk at
   `~/.pi/agent/sessions/<session>/<childRunId>/run-0/session.jsonl`.
   Parse the last assistant message per child.
3. The child-to-runId mapping is in
   `/tmp/pi-subagents-uid-1000/async-subagent-results/completion-replay/<runId>.json`,
   whose `results[]` order matches the `runs.all` item order and whose
   `outputState: "present"` confirms output was captured.

Reconstruct the result JSON from those, feed it to `rail result`, and let
`verify-worker` prove the commits. The run continues normally; nothing needs
re-running.

## Prevention

Treat the workflow return value as the least reliable part of a wave. The
durable artifacts are the worker's commit and its session log, and the
orchestrator already verifies the commit independently — so a lost return value
should cost a parse, never a re-dispatch.

Keep the return expression trivial. Any aggregation more complex than
`out.map(...)` runs after all the expensive work has completed and can only
subtract value by throwing there.
