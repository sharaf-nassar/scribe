---
title: A zsh test uname fork failure masquerades as Linux
date: 2026-08-24
component: scribe-server zsh integration tests
tags: [tests, zsh, fork, resource-pressure, integration-gate, flake]
problem_type: test-flake
---

# A zsh test uname fork failure masquerades as Linux

## Problem

Two unrelated integration gates in `run-20260824T090151.TFPnde` failed
`zsh_integration_sources_zprofile_for_non_login_shells_on_darwin` at
`crates/scribe-server/src/session_manager.rs:1933-1938`:

```text
expected ~/.zprofile to be sourced on Darwin, got output:
ZPROFILE=0 GUARD=0
```

The exact test passed alone, 30 focused three-test repetitions passed, 200
concurrent lib/bin exact-test pairs passed, and a later full gate passed. The
failure looked like the test had selected Linux instead of its Darwin fixture.

## Root cause

The production script detects Darwin with `$(uname -s 2>/dev/null)`
(`dist/shell-integration/zsh/scribe.zsh:18`). The old test put a fake `uname`
on `PATH`, but zsh still had to fork that helper. Under a deliberately low
process limit, the worker reproduced `fork failed: resource temporarily
unavailable`, followed by the exact `ZPROFILE=0 GUARD=0` signature.

Two details hid the real failure:

- the production probe redirects `uname` stderr to `/dev/null`;
- the test ran `source ...; printf ...`, so the successful `printf` replaced
  the failed source status and the child appeared to exit successfully.

The fixture therefore converted resource pressure into valid-looking
non-Darwin output rather than a process error.

## What didn't work

Timestamp-directory collision was plausible because the helper used a clock
nonce, but focused threaded and cross-binary stress did not reproduce it.
Repeating the exact test also could not exercise the failing boundary: the fork
only failed under broader process pressure. Treating a clean exact rerun as
proof of a transient gate would have left the masked subprocess failure intact.

## Fix

`scribe-e0fv` landed as `bc47c4d`. The tests now read the shipped script and
replace its one exact `ZSH_UNAME_PROBE` occurrence
(`crates/scribe-server/src/session_manager.rs:1573, 1974-1982`) inside a
process-unique private copy. The sourced script keeps its real control flow but
no longer needs a nested test-owned `uname` process.

`run_zsh_integration_check` now preserves the source status and reports the
child's status and stderr (`crates/scribe-server/src/session_manager.rs:1984-
2010`). Its temporary home uses PID plus an atomic sequence and exclusive
creation (`crates/scribe-server/src/session_manager.rs:2013-2023`). The
production script was not changed. Focused lib/bin tests, 50 concurrent pairs,
two full `just test` runs, clippy, pre-commit, and `lat check` passed.

## Prevention

- A test double implemented as another process can fail independently from the
  code under test. Prefer an in-process seam when the subprocess itself is not
  the contract.
- Preserve the status of the operation being asserted; a diagnostic `printf`
  must not overwrite it.
- When production intentionally suppresses stderr, the test harness must expose
  its own child status and stderr so resource failures remain distinguishable.
- Do not conclude that a full-suite-only flake is a path collision until focused
  concurrency reproduces it. Add the missing boundary evidence first.
