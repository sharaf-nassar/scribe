---
title: A stale rlib fails the doctest stage right after a dependency rebuild
date: 2026-08-20
component: pre-commit cargo test stage, implement-ready integration
tags: [cargo, rustdoc, doctest, pre-commit, transient, integration, misattribution]
problem_type: runtime-error
---

## Problem

Integrating `scribe-4a6g` in run `run-20260820T054159.ub1CzZ`, the pre-commit
`cargo test` stage failed and blocked the commit:

```text
   Doc-tests scribe_client
error[E0463]: can't find crate for `scribe_common`
  --> crates/scribe-client/src/ai_indicator.rs:22:5
error[E0463]: can't find crate for `scribe_server`
  --> crates/scribe-client/src/lan_dial.rs:39:5
error: doctest failed, to rerun pass `-p scribe-client --doc`
```

Every real test had just passed in the same run — 365 lib, 261 bin, 9
integration, 2 in `scribe-test`. Only the doctest stage failed, and it failed on
a crate the staged change did not touch: the commit was one function in
`crates/scribe-server/src/git_ref_watcher.rs` plus two `lat.md` files.

## Root cause

`rustdoc` is invoked with fully-resolved `--extern` paths to specific hashed
rlibs:

```text
--extern scribe_common=…/target/debug/deps/libscribe_common-b48cad5dc8f35622.rlib
--extern scribe_server=…/target/debug/deps/libscribe_server-0225a5ac6a92f946.rlib
```

Staging a `scribe-server` source change invalidates `scribe-server` and
everything downstream. The rebuild replaces those artifacts with
differently-hashed ones, and the doctest invocation is left pointing at paths
that no longer exist. It is an artifact-staleness race in the build directory,
not a compile error in any source file.

A plain re-run fixes it, because the second invocation resolves against the
rebuilt artifacts:

```text
cargo test -p scribe-client --doc
    Finished `test` profile … in 13.49s
   Doc-tests scribe_client
test result: ok. 0 passed; 0 failed  EXIT=0
```

## Fix

Re-run the failing stage. Under the implement-ready rail this is the
**transient** failure class — the only one where a plain re-run is legitimate,
because the condition clearing *is* the concrete change.

Before re-running, confirm the prepared state is still intact rather than
rebuilding it:

```bash
git write-tree                 # must equal prepare's staged_tree
git status --porcelain         # entries should be "M " (staged), not " M"
git log --oneline -1           # HEAD must not have moved
```

`cargo test` cannot modify tracked sources, so the tree hash matching
`prepare`'s recorded `staged_tree` is sufficient proof that only a re-commit is
needed. The integration lock is still held throughout; do not `unlock --abort`
for this.

## Prevention

The real hazard is misattribution. This failure appears at exactly the moment a
task's commit lands, so it looks like the task broke the build — the same shape
as a genuine `verify-integration` exit 10, which *does* belong to the task that
just landed. Distinguish them by what failed: a transient rlib staleness fails
only the doctest stage while every real test passes, and it names a crate the
diff never touched. A genuine exit 10 fails real tests in code reachable from
the change.

Reproduce the failing stage alone before concluding anything. One targeted
`cargo test -p <crate> --doc` separates the two in under a minute and costs far
less than reverting a correct commit.
