---
title: Annotated tag pushes emit no CI push event
date: 2026-08-19
component: scribe-server git_ref_watcher, github_ci
tags: [git, for-each-ref, annotated-tag, peeling, github-actions, ci-bar, degenerate-fixture, test-oracle]
problem_type: bug
---

## Problem

In `~/work/quill`: `git push` on main, then `./release.sh bump patch`. The
branch CI bar appeared and tracked correctly. The release workflow — triggered
seconds later by the tag push at the *same* commit, while the branch run was
still going — never produced a bar at all, and the server never issued a
request for it.

Both runs existed on GitHub for the one head:

    gh api --method GET repos/sharaf-nassar/quill/actions/runs \
      -f head_sha=4bc762cbd156ec6e89f3374c2f3427f90cc2a3a8 \
      -f event=push -f per_page=100

    id 32315001392  workflow_id 237563504  Release  v0.3.46  in_progress
    id 32314794466  workflow_id 280029326  ci       main     success

## Root cause

`git for-each-ref --format='%(objectname)'` on an **annotated** tag returns the
*tag object* SHA, not the commit the tag points at. Only `%(*objectname)` peels
to the commit, and it is empty for a lightweight tag.

    $ git for-each-ref --format='%(refname)%00%(objectname)%00%(*objectname)' refs/tags/v0.3.46
    refs/tags/v0.3.46  cb4a072178337c1941ab02fa1919eba98c30863b  4bc762cbd156ec6e89f3374c2f3427f90cc2a3a8

`logical_snapshot` snapshots `refs/tags` with the unpeeled `%(objectname)`
(`crates/scribe-server/src/git_ref_watcher.rs:135-140`). `detect_pushes` puts
those tag OIDs into `same_oid_generations` and then tests membership against
the remote-tracking tip OID — always a commit — to decide whether an *unchanged*
remote-tracking ref still qualifies a push generation
(`crates/scribe-server/src/git_ref_watcher.rs:203-252`). For an annotated tag
the two sets are disjoint by construction, so no `PushDetected` is emitted and
`GithubCiTracker` never opens a window.

A tag push updates no remote-tracking ref, so the local tag write is the only
signal there is. Losing it loses the event entirely — there is no second chance
further down the pipeline.

The same-OID generation trigger exists precisely to catch this flow; it was
added by bead `scribe-sz46` for quill's retag sequence. It has never worked in
production, because `release.sh` only ever creates annotated tags
(`git tag -a`), and the shipped test used a lightweight one:

    // crates/scribe-server/src/git_ref_watcher.rs:868
    run(&fixture.work, ["tag", "release"]);   // lightweight: objectname IS the commit

Lightweight and annotated tags collapse onto the same OID in that fixture, so
the assertion passed while the only shape the product actually encounters
failed. Same failure mode as
`docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md`: the
fixture picks the one input where two distinct behaviours produce one value.

## What didn't work

- **Blaming the tracker's generation cutoff first.** `GithubCiTracker` does
  have a real second defect here — the repository-wide run-id cutoff at
  `crates/scribe-server/src/github_ci.rs:1020` discards concurrent runs at the
  same head, filed as `scribe-h58f`. But it is downstream and masked: with no
  push event, the tracker is never reached. Tracing the timeline from the
  filesystem outward (`stat` on `.git/refs/...` vs the GitHub run `createdAt`)
  is what separated the two.
- **Assuming the tag push itself is observable.** `git push origin <tag>`
  writes nothing under `refs/remotes/`, so watching remote-tracking refs alone
  cannot see it. The local `refs/tags/<name>` write, which happens a few
  hundred milliseconds earlier under the same debounce, is the whole signal.

## Fix

Peel the tag snapshot so annotated and lightweight tags yield the same OID:

    %(refname)%00%(if)%(*objectname)%(then)%(*objectname)%(else)%(objectname)%(end)

`%(*objectname)` is empty for a lightweight tag, so the `%(if)` falls back to
the direct OID and `parse_refs` still sees exactly two NUL-separated fields.
Verified against the failing annotated-tag test and all 13 existing
`git_ref_watcher` tests. Filed as `scribe-4a6g`; the downstream display half is
`scribe-h58f`. Both unlanded as of this writing.

## Prevention

Any `for-each-ref`, `ls-remote`, or `show-ref` output covering `refs/tags` must
peel. `%(objectname)` answers "what object is this ref", which for an annotated
tag is not the commit. Reach for `%(*objectname)` with a fallback, or compare
against `<ref>^{}`.

Tag fixtures need `git tag -a`, not `git tag`. A lightweight tag is the
degenerate case where the peeled and unpeeled OIDs coincide, so it cannot
distinguish peeling from not peeling. Where a repo's own tooling creates
annotated tags, the test that guards that tooling has to create one too.

When a ref-watching feature is reported as silently doing nothing, reconstruct
the timeline from the filesystem before reading tracker logic: `stat -c '%y'`
on the specific `.git/refs/...` paths, against the provider's `createdAt`
timestamps. That ordering says which stage dropped the event, and stops a
downstream defect from being mistaken for the cause.
