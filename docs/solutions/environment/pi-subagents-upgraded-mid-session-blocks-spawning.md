---
title: Upgrading pi-subagents mid-session blocks every agent spawn
date: 2026-08-19
component: pi-subagents, implement-ready orchestration
tags: [pi, subagents, extensions, npm, version-skew, orchestration, startup]
problem_type: environment
---

## Problem

Every worker dispatched in implement-ready run `run-20260818T211904.Pion6e`
died instantly, before running a single tool, with:

```text
Agent 'gpt-pro' has invalid runner.type; expected 'pi' or 'external-cli'.
```

The failure was total, not per-agent. `subagent({action:"list"})` returned the
same error, and a `runs.all` of two unrelated `worker` children returned that
string as *both* children's output. Nothing about the run's tasks was involved.

## Root cause

Version skew between the loaded extension and the on-disk agent definitions.

The `pi` process started at 14:15:53. `pi-subagents` was upgraded on disk at
14:17:05, 72 seconds later. So the process held the pre-0.50 loader in memory
while the filesystem carried 0.51.0's agent files.

Those two disagree about one enum. The 0.51.0 loader accepts three runner
types (`src/agents/agents.ts:1531` — "expected 'pi', 'external-cli', or
'external-job'"), and 0.51.0 ships `agents/gpt-pro.md` declaring
`runner.type: external-job`. The older in-memory loader knows only two, so it
throws on that file.

The blast radius is what makes this expensive: agent `.md` files are read from
disk at spawn time, not at process start, and one unparseable file aborts the
whole agent registry rather than being skipped. So a package upgrade that
touches any agent definition disables *all* subagent spawning in every already
-running `pi` process, with an error message that names an agent nobody asked
for.

Grepping for the error string is misleading. It appears nowhere in the
installed package — both the 0.50.0 and 0.51.0 sources on disk emit the newer
three-type message. The only copies are in session logs, because the code
producing it exists solely in process memory.

## Fix

Restart `pi`. The on-disk 0.51.0 loader handles `external-job` correctly.

To recover a run already in flight without losing its session, move the single
offending agent definition aside:

```bash
mv ~/.pi/agent/npm/node_modules/pi-subagents/agents/gpt-pro.md \
   ~/.pi/agent/npm/node_modules/pi-subagents/agents/gpt-pro.md.disabled
```

`subagent({action:"list"})` then lists the remaining agents and spawning works
again. Restore the file afterwards — it is valid, and the next restart wants
it. This only helps when the skewed definitions are ones the run does not need.

## Prevention

Do not upgrade `pi` packages while a `pi` session that will spawn subagents is
open. The failure surfaces at the first spawn, which in an orchestrated run can
be well after the upgrade, and it is easy to misread as a defect in the task or
the agent config.

When a dispatch fails with a message about an agent the run never referenced,
check process start time against package mtime before touching anything else:

```bash
ps -o lstart= -p "$(pgrep -x pi)"
stat -c '%y' ~/.pi/agent/npm/node_modules/pi-subagents/package.json
```

If the package is newer than the process, it is skew, not configuration.
