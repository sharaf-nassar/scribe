---
title: The rail's overlap guard sees nothing until the other task has a worktree
date: 2026-08-20
component: implement-ready orchestration
tags: [orchestration, rail, overlap, worktree, claim, beads, conflict]
problem_type: convention
---

## Problem

In run `run-20260820T054159.ub1CzZ` two tasks were about to be dispatched in one
wave. `overlap` was run for both up front and both came back:

```json
{"task_id":"scribe-1mpq","hub_contention":[],"conflicts":[],"status":"clear"}
```

Their declared `Files:` lines plainly collided — both listed
`crates/scribe-client/src/main.rs` and `lat.md/client.md`. Taking the guard at
its word would have put two writers on the same two files in parallel.

## Root cause

`cmd_overlap` compares the candidate only against `active_tasks`, and that
function enumerates `"$run_dir"/tasks/*.json` — the records written by
`worktree`, not by `claim`:

```bash
active_tasks() {
  for f in "$run_dir"/tasks/*.json; do
    task="$(basename "$f" .json)"
    [[ -e "$(cleanup_path "$run_dir" "$task")" ]] || printf '%s\n' "$task"
  done
}
```

So a task that is claimed but has no worktree yet is invisible to the guard.
Running `overlap` for every candidate before creating any worktree — the
natural way to plan a wave — compares each one against an empty set and always
returns `clear`.

The sequence proves it. Same pair, three calls:

| when | result |
| --- | --- |
| before either is claimed | `clear` |
| after 1xvr is **claimed** | `clear` |
| after 1xvr has a **worktree** | `conflict` |

The third call finally reported the truth:

```json
{"hub_contention":["scribe-1xvr:crates/scribe-client/src/main.rs"],
 "conflicts":["scribe-1xvr:lat.md/client.md"],"status":"conflict"}
```

Note the split: `main.rs` is a hub file (`is_hub_file` lists `main.rs`,
`lib.rs`, `mod.rs`, `index.ts`, `Cargo.toml`, …) so it downgrades to
`hub_contention`, while `lat.md/client.md` is an ordinary file and is a real
blocking `conflict`.

## Fix

Interleave per task rather than batching the guard:
`overlap → claim → worktree`, then `overlap` the *next* candidate. The guard is
only meaningful once every earlier task in the wave already holds a worktree.

Cheaper still, and independent of the rail: compare the declared `Files:` sets
yourself before planning the wave. `survey` returns `files` per ready task, so
the collision is visible with no rail call at all. Treat the guard as
confirmation, not discovery.

When the conflict is real, serialize: dispatch one, integrate it, then claim the
second off the updated main. In this run that also removed the conflict
entirely, because `cleanup` retires the first task's worktree before the second
one is planned.

## Prevention

Two smaller rail facts worth knowing, both learned the same way:

- `cleanup` refuses until the bead is **closed**, so the order is
  `close → cleanup → unlock`, not the reverse.
- `bd close` refuses a bead the rail holds: `assignee is
  "codex-implement-ready-run-…", actor is "<you>"`. Close it as the run actor,
  `bd --actor "$(jq -r .actor <manifest>)" close <id>`. Never `--force`; that
  would be stealing the rail's own claim.
