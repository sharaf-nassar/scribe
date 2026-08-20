---
title: bd update --notes overwrites the whole notes field
date: 2026-08-20
component: beads CLI, review-followup bookkeeping
tags: [beads, bd, notes, data-loss, review, orchestration]
problem_type: environment
---

## Problem

During the ponytail-review disposition of run `run-20260820T054159.ub1CzZ`, two
findings were recorded onto the same bead in two separate calls:

```bash
bd update scribe-1c4x --notes "<xdotool find/focus duplication>"     # earlier
bd update scribe-1c4x --notes "<fail/shot/delta duplication>"        # later
```

The second call destroyed the first. Worse, the surviving text opened with "the
find/focus idiom already noted above" — a reference to content that no longer
existed, so the bead read as though a section had been lost without saying
which.

## Root cause

`--notes` **sets** the field; it does not append. `bd` does say so, but only
after the write has already happened:

```text
warning: scribe-1c4x: --notes replaced existing notes
         (use --append-notes to preserve history)
```

It is a warning on a completed destructive write, not a refusal, and it is easy
to miss in a batch of orchestration output. There is no confirmation prompt and
no undo.

## Fix

Use `--append-notes` whenever a bead may already carry notes:

```bash
bd update <id> --append-notes "<new observation>"
```

Reserve `--notes` for the case where replacing the entire field is the actual
intent.

If it has already happened, the old text is not recoverable from `bd` — rewrite
the field with both sections in one `--notes` call, then verify:

```bash
bd show <id> --json | jq -r '.[0].notes'
```

## Prevention

This bites hardest exactly where notes matter most: a bead that accumulates
findings across several runs. A review disposition, a stuck-task explanation,
and a retry rationale are all appended to beads that already have history, and
all three are written by an orchestrator running many `bd` calls in sequence
where one more warning line scrolls past unread.

Default to `--append-notes` and treat `--notes` as the special case, not the
other way round.
